import assert from "node:assert/strict";
import test from "node:test";
import {
  BrowserNetworkClient,
  encryptPublicFile,
  parseWebRtcDirectMultiaddr,
} from "./pkg/ant_core.js";
import { mockWebRtc, paymentNetwork } from "./mock-webrtc.mjs";

const content = new TextEncoder().encode("Upload quorum regression fixture.".repeat(100));

function wallet() {
  const calls = [];
  return {
    calls,
    async pay(network, quotes) {
      calls.push({ network, quotes });
      return {
        transactionHash: `0x${"ab".repeat(32)}`,
        totalAmount: quotes.reduce((sum, quote) => sum + BigInt(quote.amount), 0n).toString(),
      };
    },
  };
}

async function upload(client, signer) {
  return client.uploadPublicFile(content, "fixture.txt", "text/plain", paymentNetwork, signer.pay);
}

for (const existing of [1, 3]) {
  test(`${existing} already-stored votes still require majority replication`, async () => {
    const rtc = mockWebRtc(Array.from({ length: 4 }, (_, i) => ({ alreadyStored: i < existing })));
    const client = new BrowserNetworkClient(rtc.endpoints);
    const signer = wallet();
    try {
      const result = await upload(client, signer);
      assert.equal(signer.calls.length, 1);
      assert.equal(result.file.replicas, 4);
      assert.equal(rtc.requests.filter(({ method }) => method === "put_chunk").length, result.records * 4);
    } finally {
      client.close();
    }
  });
}

test("four distinct already-stored votes skip both payment and PUTs", async () => {
  const rtc = mockWebRtc(Array.from({ length: 7 }, (_, i) => ({ alreadyStored: i < 4 })));
  const client = new BrowserNetworkClient(rtc.endpoints);
  const signer = wallet();
  try {
    const result = await upload(client, signer);
    assert.equal(signer.calls.length, 0);
    assert.equal(result.storageCostAtto, "0");
    assert.equal(result.file.replicas, 4);
    assert.equal(rtc.requests.filter(({ method }) => method === "put_chunk").length, 0);
  } finally {
    client.close();
  }
});

for (const count of [1, 2, 3]) {
  test(`${count} discovered targets fail before any payment`, async () => {
    const rtc = mockWebRtc(Array.from({ length: count }, () => ({})));
    const client = new BrowserNetworkClient(rtc.endpoints);
    const signer = wallet();
    try {
      await assert.rejects(upload(client, signer), /need 4 before payment/);
      assert.equal(signer.calls.length, 0);
      assert.equal(rtc.requests.filter(({ method }) => method === "put_chunk").length, 0);
    } finally {
      client.close();
    }
  });
}

test("duplicate endpoints cannot satisfy the distinct-peer minimum", async () => {
  const rtc = mockWebRtc([{ alreadyStored: true }]);
  const client = new BrowserNetworkClient(Array(4).fill(rtc.endpoints[0]));
  const signer = wallet();
  try {
    await assert.rejects(upload(client, signer), /only 1 eligible.*need 4 before payment/);
    assert.equal(signer.calls.length, 0);
    assert.equal(rtc.requests.filter(({ method }) => method === "put_chunk").length, 0);
  } finally {
    client.close();
  }
});

for (const ineligible of [{ uploads: false }, { invalidQuote: true, alreadyStored: true }]) {
  test(`ineligible storage target (${JSON.stringify(ineligible)}) prevents payment`, async () => {
    const rtc = mockWebRtc([{}, {}, {}, ineligible]);
    const client = new BrowserNetworkClient(rtc.endpoints);
    const signer = wallet();
    try {
      await assert.rejects(upload(client, signer), /only 3 eligible.*need 4 before payment/);
      assert.equal(signer.calls.length, 0);
      assert.equal(rtc.requests.filter(({ method }) => method === "put_chunk").length, 0);
    } finally {
      client.close();
    }
  });
}

test("an ineligible peer is excluded when four valid storage targets remain", async () => {
  const rtc = mockWebRtc([{}, {}, {}, {}, { uploads: false }]);
  const client = new BrowserNetworkClient(rtc.endpoints);
  const signer = wallet();
  try {
    const result = await upload(client, signer);
    assert.equal(result.file.replicas, 4);
    assert.equal(signer.calls.length, 1);
    assert.equal(rtc.requests.filter(({ node, method }) => node === 4 && method === "put_chunk").length, 0);
  } finally {
    client.close();
  }
});

const priceCases = [
  { name: "expensive closest peer", nodes: Array.from({ length: 7 }, () => ({})), closestCount: 1_000_000, expectedCount: 0 },
  { name: "cheap closest peer", nodes: Array.from({ length: 7 }, () => ({ keyCount: 6000 })), closestCount: 0, expectedCount: 6000 },
  { name: "seven different prices", nodes: [7000, 1000, 6000, 2000, 5000, 3000, 4000].map(keyCount => ({ keyCount })), expectedCount: 4000 },
  { name: "upper median of four", nodes: [6000, 1000, 4000, 2000].map(keyCount => ({ keyCount })), expectedCount: 4000 },
  { name: "stable ties", nodes: Array.from({ length: 4 }, () => ({ keyCount: 23 })), expectedCount: 23, tied: true },
  { name: "already-stored quote is excluded", nodes: [1000, 2000, 3000, 4000].map(keyCount => ({ keyCount, alreadyStored: keyCount === 3000 })), expectedCount: 2000 },
  { name: "invalid cheap quote is excluded", nodes: [{ keyCount: 0, invalidQuote: true }, ...[1000, 2000, 4000, 6000].map(keyCount => ({ keyCount }))], expectedCount: 4000 },
];

for (const scenario of priceCases) {
  for (const staged of [false, true]) {
    test(`${staged ? "staged" : "buffered"} upload pays the shared median: ${scenario.name}`, async () => {
      const encrypted = encryptPublicFile(content);
      const options = scenario.nodes.map(node => ({ ...node }));
      const rtc = mockWebRtc(options);
      const peerIds = rtc.endpoints.map(endpoint => parseWebRtcDirectMultiaddr(endpoint).peerId);
      const closestFirst = address => peerIds.map((peer, index) => ({
        index,
        distance: BigInt(`0x${peer}`) ^ BigInt(`0x${address}`),
      })).sort((a, b) => a.distance < b.distance ? -1 : a.distance > b.distance ? 1 : 0);
      if (scenario.closestCount !== undefined) {
        // Reproduce the original defect for the first record regardless of
        // the deterministic mock peers' positions in XOR space.
        options[closestFirst(encrypted.records[0].address)[0].index].keyCount = scenario.closestCount;
      }
      const client = new BrowserNetworkClient(rtc.endpoints);
      const signer = wallet();
      try {
        const result = staged
          ? await client.uploadStagedPublicFile({
            ...encrypted,
            name: "fixture.txt",
            content_type: "text/plain",
            size: content.length,
            records: encrypted.records.map(record => ({ address: record.address, size: record.content.length })),
          }, paymentNetwork, async index => encrypted.records[index].content, signer.pay)
          : await upload(client, signer);
        assert.equal(signer.calls.length, 1);
        assert.equal(result.file.replicas, 4);
        const quotes = signer.calls[0].quotes;
        assert.equal(quotes.length, result.records);
        // Fixed native price curve, independent of the implementation's
        // selected amount and reported total.
        const count = BigInt(scenario.expectedCount);
        const expectedAmount = 3n * (3_906_250_000_000_000n + count * count * 35_156_250_000_000_000n / 36_000_000n);
        assert.equal(result.storageCostAtto, (expectedAmount * BigInt(result.records)).toString());
        for (const paid of quotes) {
          assert.equal(paid.quote.committed_key_count, scenario.expectedCount);
          assert.equal(paid.amount, expectedAmount.toString());
          if (scenario.tied) {
            assert.equal(paid.quote.peer_id, peerIds[closestFirst(paid.quote.content)[2].index]);
          }
          const puts = rtc.requests.filter(request => request.method === "put_chunk" && request.address === paid.quote.content);
          assert.equal(puts.length, 4);
          // The four primary stores run concurrently, so their observed RPC
          // arrival order can differ from the target order.
          assert.ok(puts.some(put => peerIds[put.node] === paid.quote.peer_id), "paid issuer must be in the initial storage quorum");
          for (const put of puts) {
            assert.equal(put.quoteHash, paid.quoteHash, "storage proof must use the paid quote");
            assert.notEqual(options[put.node].invalidQuote, true);
          }
        }
      } finally {
        client.close();
      }
    });
  }
}
