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
