import { BrowserTestNode, parseWebRtcDirectMultiaddr } from "./pkg/ant_core.js";

export const paymentNetwork = {
  chain_id: 31337,
  payment_token_address: `0x${"11".repeat(20)}`,
  payment_vault_address: `0x${"22".repeat(20)}`,
};

// Reset for each test. Only browser-owned RTC objects are mocked; all node
// responses use the shared Rust wire contract and real session encryption.
export function mockWebRtc(nodes = [{}]) {
  const connections = [];
  const requests = [];
  const stores = nodes.map(() => new Map());
  const endpoints = nodes.map((options, index) => {
    const node = new BrowserTestNode(index + 1, options.alreadyStored ?? false);
    const endpoint = node.endpoint();
    node.free();
    return endpoint;
  });

  class Channel {
    readyState = "open";
    bufferedAmount = 0;
    emit(data) {
      this.onmessage?.({ data });
    }
    send(message) {
      const response = this.server.push(message);
      if (!response.length) return;
      const method = this.server.last_method();
      if (method === "put_chunk") {
        const address = this.server.last_put_address();
        const content = this.server.stored_record(address);
        if (content.length) stores[this.index].set(address, content);
      }
      requests.push({ node: this.index, method, ...(method === "put_chunk" ? {
        address: this.server.last_put_address(),
        quoteHash: this.server.last_put_quote_hash(),
      } : {}) });
      if (this.options.respond?.(this, method, response) === false) return;
      setTimeout(() => {
        for (let offset = 0; offset < response.length; offset += 16_384) {
          this.emit(response.slice(offset, offset + 16_384).buffer);
        }
      }, this.options.delay?.(method) ?? 0);
    }
    close() {
      this.readyState = "closed";
    }
  }

  globalThis.RTCPeerConnection = class {
    constructor() {
      connections.push(this);
    }
    createDataChannel() {
      return (this.channel = new Channel());
    }
    async createOffer() {
      return {
        sdp: "v=0\r\na=ice-ufrag:browserUfrag\r\na=ice-pwd:browserClientPassword1234\r\n",
      };
    }
    async setLocalDescription(local) {
      this.localDescription = local;
    }
    async setRemoteDescription(remote) {
      const seed = Number(remote.sdp.match(/m=application (\d+)/)[1]) - 24_000;
      this.channel.index = seed - 1;
      this.channel.options = nodes[seed - 1];
      if (this.channel.options.connectError) throw new Error(this.channel.options.connectError);
      this.channel.server = new BrowserTestNode(
        seed,
        this.channel.options.alreadyStored ?? false,
      );
      this.channel.server.set_chunk(this.channel.options.chunk ?? new Uint8Array());
      for (const [address, content] of stores[seed - 1]) {
        this.channel.server.set_record(address, content);
      }
      this.channel.server.set_uploads_enabled(this.channel.options.uploads ?? true);
      this.channel.server.set_invalid_quote(this.channel.options.invalidQuote ?? false);
      this.channel.server.set_committed_key_count(this.channel.options.keyCount ?? 0);
      const view = this.channel.options.view ?? nodes.map((_, i) => i);
      this.channel.server.set_closest_peers(this.channel.options.peers ?? view.map(i => ({
        peer_id: parseWebRtcDirectMultiaddr(endpoints[i]).peerId,
        native_addresses: [], reliability: 1, webrtc_direct: { multiaddr: endpoints[i] },
      })));
      if (this.channel.options.putError) {
        this.channel.server.set_put_error(this.channel.options.putError.code, this.channel.options.putError.message);
      }
      setTimeout(() => this.channel.onopen?.({}), 0);
    }
    close() {
      this.closed = true;
    }
  };
  return { endpoints, connections, requests, stores };
}
