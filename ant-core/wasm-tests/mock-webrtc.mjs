import { BrowserTestNode, parseWebRtcDirectMultiaddr } from "./pkg/ant_core.js";

export const paymentNetwork = {
  chain_id: 31337,
  payment_token_address: `0x${"11".repeat(20)}`,
  payment_vault_address: `0x${"22".repeat(20)}`,
};

// DataChannel closes still in flight (see Channel.close below).
const pendingCloses = new Set();

// Await this before asserting that a channel, or the peer connection its last
// close releases, has closed. It also waits for closes requested while it
// waits, such as sibling channels closed in reaction to a close event.
export async function closesSettled() {
  do {
    await Promise.all(pendingCloses);
    await new Promise(resolve => setImmediate(resolve));
  } while (pendingCloses.size);
}

// Reset for each test. Only browser-owned RTC objects are mocked; all node
// responses use the shared Rust wire contract and real session encryption.
// ICE gathering completes `gatheringMs` after setLocalDescription; Infinity
// never completes.
export function mockWebRtc(nodes = [{}], { gatheringMs = 0 } = {}) {
  const connections = [];
  const requests = [];
  // When each connection started and completed ICE gathering.
  const gathering = [];
  const stores = nodes.map(() => new Map());
  const endpoints = nodes.map((options, index) => {
    const node = new BrowserTestNode(index + 1, options.alreadyStored ?? false);
    const endpoint = node.endpoint();
    node.free();
    return endpoint;
  });

  class Channel extends EventTarget {
    readyState = "open";
    bufferedAmount = 0;
    emit(data) {
      this.onmessage?.({ data });
    }
    send(message) {
      // A failed association delivers nothing, while WebKit still accepts sends.
      if (this.connection.connectionState === "failed") return;
      // A real DataChannel copies the outgoing bytes. The mock re-enters WASM
      // instead, where allocating the server argument can grow memory and
      // detach an outgoing view into that same memory. Copy before re-entry,
      // including loading the node's records below.
      const outgoing = new Uint8Array(message);
      // One node has one store, whichever of its channels wrote to it.
      for (const [key, content] of stores[this.index]) this.server.set_record(key, content);
      let response = this.server.push(outgoing);
      if (!response.length) return;
      const method = this.server.last_method();
      if (method === "put_chunk") {
        const address = this.server.last_put_address();
        const content = this.server.stored_record(address);
        if (content.length) stores[this.index].set(address, content);
      }
      if (method === "put_pointer") {
        const key = `pointer:${this.server.last_put_address()}`;
        const record = this.server.stored_record(key);
        if (record.length) stores[this.index].set(key, record);
      }
      const put = method === "put_chunk" || method === "put_pointer";
      const address = method === "get_chunk" ? this.server.last_get_address() : undefined;
      requests.push({ node: this.index, method, ...(put ? {
        address: this.server.last_put_address(),
        quoteHash: this.server.last_put_quote_hash(),
      } : {}), ...(address ? { address } : {}) });
      if (this.options.respond?.(this, method, response) === false) return;
      setTimeout(() => {
        if (this.options.multiplex && method !== "handshake") response = this.server.seal_response(response);
        this.options.delivered?.(this, method);
        for (let offset = 0; offset < response.length; offset += 16_384) {
          this.emit(response.slice(offset, offset + 16_384).buffer);
        }
      }, this.options.delay?.(method, address) ?? 0);
    }
    // Matches node-datachannel's polyfill, which the Node.js WASM client
    // runs on: close() only records the request and defers the native close
    // through setImmediate, and readyState stays "open" until the native
    // callback and a second setImmediate have run. Browsers report "closing"
    // at once, so a caller that trusts readyState after close() works there
    // and spins here (V2-1305).
    close() {
      if (this.readyState === "closed" || this.closeRequested) return;
      this.closeRequested = true;
      const closed = new Promise(resolve => setImmediate(() => setImmediate(() => {
        try {
          this.finishClose();
        } finally {
          resolve();
        }
      })));
      pendingCloses.add(closed);
      closed.then(() => pendingCloses.delete(closed));
    }
    finishClose() {
      this.readyState = "closed";
      this.dispatchEvent(new Event("close"));
      this.onclose?.({});
      if (this.connection.channels.every(channel => channel.readyState === "closed")) this.connection.close();
    }
  }

  globalThis.RTCPeerConnection = class {
    connectionState = "new";
    iceGatheringState = "new";
    constructor() {
      connections.push(this);
      this.channels = [];
    }
    // WebKit reports a lost association as failed but leaves its channels
    // "open" without a close event, and opens channels created on it later.
    failLikeWebKit() {
      this.connectionState = "failed";
      this.onconnectionstatechange?.(new Event("connectionstatechange"));
    }
    createDataChannel() {
      if (this.closed) throw new Error("peer connection closed");
      const channel = new Channel();
      channel.connection = this;
      this.channels.push(channel);
      this.channel ??= channel;
      if (this.remoteSeed !== undefined) {
        void this.prepareChannel(channel).catch(() => channel.close());
      }
      return channel;
    }
    async createOffer() {
      return {
        sdp: "v=0\r\na=ice-ufrag:browserUfrag\r\na=ice-pwd:browserClientPassword1234\r\n",
      };
    }
    async setLocalDescription(local) {
      const span = { start: performance.now() };
      gathering.push(span);
      this.iceGatheringState = "gathering";
      if (Number.isFinite(gatheringMs)) {
        setTimeout(() => {
          this.iceGatheringState = "complete";
          span.complete = performance.now();
        }, gatheringMs);
      }
      this.localDescription = local;
    }
    async setRemoteDescription(remote) {
      this.remoteSeed = Number(remote.sdp.match(/m=application (\d+)/)[1]) - 24_000;
      await this.prepareChannel(this.channel);
    }
    async prepareChannel(channel) {
      channel.index = this.remoteSeed - 1;
      channel.options = nodes[this.remoteSeed - 1];
      if (channel.options.connectErrorDelay) await new Promise(resolve => setTimeout(resolve, channel.options.connectErrorDelay));
      if (channel.options.connectError) throw new Error(channel.options.connectError);
      channel.server = new BrowserTestNode(
        this.remoteSeed,
        channel.options.alreadyStored ?? false,
      );
      channel.server.set_chunk(channel.options.chunk ?? new Uint8Array());
      for (const [address, content] of stores[this.remoteSeed - 1]) {
        channel.server.set_record(address, content);
      }
      channel.server.set_multiplex(channel.options.multiplex ?? false);
      channel.server.set_address_v2(channel.options.addressV2 ?? false);
      channel.server.set_uploads_enabled(channel.options.uploads ?? true);
      channel.server.set_pointers_enabled(channel.options.pointers ?? true);
      if (channel.options.payment) channel.server.set_hello_payment(channel.options.payment);
      channel.server.set_invalid_quote(channel.options.invalidQuote ?? false);
      channel.server.set_committed_key_count(channel.options.keyCount ?? 0);
      const view = channel.options.view ?? nodes.map((_, i) => i);
      channel.server.set_closest_peers(channel.options.peers ?? view.map(i => ({
        peer_id: parseWebRtcDirectMultiaddr(endpoints[i]).peerId,
        native_addresses: [], reliability: 1, webrtc_direct: { multiaddr: endpoints[i] },
      })));
      if (channel.options.putError) {
        channel.server.set_put_error(channel.options.putError.code, channel.options.putError.message);
      }
      if (channel.options.connectDelay) await new Promise(resolve => setTimeout(resolve, channel.options.connectDelay));
      if (this.connectionState === "new") this.connectionState = "connected";
      setTimeout(() => channel.onopen?.({}), 0);
    }
    close() {
      if (this.closed) return;
      this.closed = true;
      // Browsers report closed at once, without a connectionstatechange event.
      this.connectionState = "closed";
      for (const channel of this.channels) channel.close();
    }
  };
  return { endpoints, connections, requests, stores, gathering };
}
