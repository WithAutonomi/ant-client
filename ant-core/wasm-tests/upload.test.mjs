import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./pkg/ant_core.js";
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
