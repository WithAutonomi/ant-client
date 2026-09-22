import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient, contentAddress, encryptPublicFile } from "./pkg/ant_core.js";
import { mockWebRtc, paymentNetwork } from "./mock-webrtc.mjs";

const content = new TextEncoder().encode("Payment recovery regression. ".repeat(100));
const hash = `0x${"ab".repeat(32)}`;
const receipt = quotes => ({
  transactionHash: hash,
  totalAmount: quotes.reduce((sum, quote) => sum + BigInt(quote.amount), 0n).toString(),
});
const merkleReceipt = request => ({
  transactionHash: hash, winnerPoolHash: request.poolHashes[0], totalAmount: request.maximumAmount,
});
const noSingle = () => { throw new Error("unexpected single payment"); };
const notSubmitted = () => ({ status: "notSubmitted", evidence: { walletRequest: "rejected before submission" } });
const reverted = transactionHashes => ({
  status: "reverted", transactionHashes, evidence: { receipts: "verified final reverts by wallet adapter" },
});

function stagedRecords() {
  const encrypted = encryptPublicFile(content);
  const records = encrypted.records.slice(0, -1);
  while (records.length < 256) {
    const bytes = new TextEncoder().encode(`valid record ${records.length}`);
    records.push({ address: contentAddress(bytes), content: bytes });
  }
  records.push(encrypted.records.at(-1));
  return { records, staged: { name: "fixture", content_type: "text/plain", address: encrypted.address,
    records: records.map(record => ({ address: record.address, size: record.content.length })) } };
}

test("later Merkle payment and recovery failures still store the first paid batch", async () => {
  const rtc = mockWebRtc(Array.from({ length: 16 }, () => ({})));
  const client = new BrowserNetworkClient(rtc.endpoints);
  const { records, staged } = stagedRecords();
  let payments = 0, checkpoint;
  const pay = async (_, request, submitted) => {
    payments++;
    if (payments === 2) {
      await submitted({ transactionHash: `0x${"cd".repeat(32)}` });
      throw new Error("second payment observation failed");
    }
    return merkleReceipt(request);
  };
  const upload = () => client.uploadStagedPublicFile(staged, paymentNetwork, i => records[i].content,
    noSingle, undefined, checkpoint, value => { checkpoint = value; }, "merkle", pay);
  try {
    // The batch partitioner avoids singleton payments by splitting 257 as 255 + 2.
    await assert.rejects(upload(), /partial upload: 255\/257 stored, 2 failed:.*second payment observation failed/);
    const puts = rtc.requests.filter(r => r.method === "put_chunk");
    assert.equal(new Set(puts.map(r => r.address)).size, 255);
    assert.equal(payments, 2);
    pay.recover = async (_, _request, _submitted, attempt) => {
      assert.equal(attempt.submissions[0].transactionHash, `0x${"cd".repeat(32)}`);
      throw new Error("still pending");
    };
    await assert.rejects(upload(), /partial upload: 255\/257 stored, 2 failed:.*still pending/);
    assert.ok(rtc.requests.filter(r => r.method === "put_chunk").length > puts.length);
    assert.equal(payments, 2, "an unresolved payment must never be resubmitted");
    checkpoint = await client.reconcileFailedUploadPayment(checkpoint,
      () => reverted([`0x${"cd".repeat(32)}`]), value => { checkpoint = value; });
    const completed = await upload();
    assert.equal(completed.records, 257);
    assert.equal(payments, 3, "retry pays only the failed batch and retains earlier proofs");
  } finally { client.close(); }
});

test("partial Merkle error preserves both payment and staged-storage failures", async () => {
  const rtc = mockWebRtc(Array.from({ length: 16 }, () => ({})));
  const client = new BrowserNetworkClient(rtc.endpoints);
  const { records, staged } = stagedRecords();
  let payments = 0;
  try {
    await assert.rejects(client.uploadStagedPublicFile(staged, paymentNetwork, i => {
      if (payments === 2) throw new Error("staged bytes evicted");
      return records[i].content;
    }, noSingle, undefined, undefined, () => {}, "merkle", async (_, request) => {
      if (++payments === 2) throw new Error("second payment rejected");
      return merkleReceipt(request);
    }), error => {
      assert.match(String(error), /partial upload/);
      assert.match(String(error), /second payment rejected/);
      assert.match(String(error), /staged bytes evicted/);
      return true;
    });
  } finally { client.close(); }
});

for (const mode of ["single", "merkle"]) {
  test(`explicit reconciliation permits retry of a rejected ${mode} payment`, async () => {
    const rtc = mockWebRtc(Array.from({ length: mode === "single" ? 7 : 16 }, () => ({})));
    const client = new BrowserNetworkClient(rtc.endpoints);
    let checkpoint, calls = 0, persisted = 0;
    const pay = async (_, request) => {
      if (++calls === 1) throw Object.assign(new Error("user rejected"), { code: 4001 });
      return mode === "single" ? receipt(request) : merkleReceipt(request);
    };
    const upload = () => client.uploadPublicFile(content, "fixture", "text/plain", paymentNetwork,
      mode === "single" ? pay : noSingle, undefined, checkpoint, value => { checkpoint = value; },
      mode, mode === "merkle" ? pay : undefined);
    try {
      await assert.rejects(upload(), /user rejected/);
      await assert.rejects(upload(), /outcome unknown/);
      assert.equal(calls, 1);
      const reconciled = await client.reconcileFailedUploadPayment(checkpoint, (attempt, scope) => {
        assert.equal(attempt.merkle, mode === "merkle");
        assert.equal(attempt.submissions.length, 0);
        assert.equal(scope, JSON.parse(checkpoint).scope);
        return notSubmitted();
      }, async value => {
        await new Promise(resolve => setTimeout(resolve, 5));
        checkpoint = value;
        persisted++;
      });
      assert.equal(reconciled, checkpoint);
      assert.equal(persisted, 1);
      assert.equal(calls, 1, "reconciliation never invokes payment");
      await upload();
      assert.equal(calls, 2);
    } finally { client.close(); }
  });

  test(`closing during an awaited ${mode} payment checkpoint prevents submission and allows retry`, async () => {
    const rtc = mockWebRtc(Array.from({ length: mode === "single" ? 7 : 16 }, () => ({})));
    let client = new BrowserNetworkClient(rtc.endpoints), checkpoint, calls = 0;
    const pay = async (_, request) => { calls++; return mode === "single" ? receipt(request) : merkleReceipt(request); };
    try {
      await assert.rejects(client.uploadPublicFile(content, "fixture", "text/plain", paymentNetwork,
        mode === "single" ? pay : noSingle, undefined, undefined, async value => {
          checkpoint = value;
          await new Promise(resolve => setTimeout(resolve, 0));
          client.close();
        }, mode, mode === "merkle" ? pay : undefined), /closed.*not submitted/);
      assert.equal(calls, 0);
      client = new BrowserNetworkClient(rtc.endpoints);
      await client.uploadPublicFile(content, "fixture", "text/plain", paymentNetwork,
        mode === "single" ? pay : noSingle, undefined, checkpoint, value => { checkpoint = value; },
        mode, mode === "merkle" ? pay : undefined);
      assert.equal(calls, 1, "definitely unsubmitted intent must not require recovery");
    } finally { client.close(); }
  });
}

test("reconciliation requires verified failure for every submitted transaction", async () => {
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({})));
  const client = new BrowserNetworkClient(rtc.endpoints);
  const secondHash = `0x${"cd".repeat(32)}`;
  let checkpoint, calls = 0, writes = 0;
  const pay = async (_, quotes, submitted) => {
    if (++calls === 1) {
      await submitted({ transactionHash: hash });
      await submitted({ transactionHash: secondHash });
      throw new Error("transactions reverted");
    }
    return receipt(quotes);
  };
  const upload = () => client.uploadPublicFile(content, "fixture", "text/plain", paymentNetwork, pay,
    undefined, checkpoint, value => { checkpoint = value; }, "single");
  try {
    await assert.rejects(upload(), /transactions reverted/);
    const original = checkpoint;
    for (const verify of [
      notSubmitted,
      () => ({ status: "pending", evidence: { timeout: true } }),
      () => reverted([hash]),
      () => ({ status: "reverted", transactionHashes: [hash, secondHash], evidence: {} }),
      () => { throw new Error("RPC unavailable"); },
    ]) {
      await assert.rejects(client.reconcileFailedUploadPayment(checkpoint, verify, () => { writes++; }));
      assert.equal(writes, 0);
      assert.equal(checkpoint, original);
    }
    await assert.rejects(client.reconcileFailedUploadPayment(checkpoint, () => reverted([hash, secondHash]),
      () => { throw new Error("disk full"); }), /disk full/);
    await assert.rejects(upload(), /outcome unknown/);
    assert.equal(calls, 1);
    checkpoint = await client.reconcileFailedUploadPayment(checkpoint, attempt => {
      assert.equal(attempt.submissions.length, 2);
      return reverted([hash, secondHash]);
    }, value => { checkpoint = value; });
    await upload();
    assert.equal(calls, 2);
    await assert.rejects(client.reconcileFailedUploadPayment(checkpoint, notSubmitted, () => {}), /no pending payment/);
  } finally { client.close(); }
});

test("confirmed receipt evidence cannot be cleared as an unsubmitted payment", async () => {
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({})));
  const client = new BrowserNetworkClient(rtc.endpoints);
  let checkpoint, writes = 0;
  try {
    await assert.rejects(client.uploadPublicFile(content, "fixture", "text/plain", paymentNetwork,
      async (_, quotes) => ({ ...receipt(quotes), totalAmount: "0" }), undefined, undefined,
      value => { checkpoint = value; }, "single"), /different payment total/);
    await assert.rejects(client.reconcileFailedUploadPayment(checkpoint, notSubmitted, () => { writes++; }),
      /submission evidence exists/);
    assert.equal(writes, 0);
  } finally { client.close(); }
});
