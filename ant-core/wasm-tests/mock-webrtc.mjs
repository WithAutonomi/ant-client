import { BrowserTestNode } from "./pkg/ant_core.js";

export const paymentNetwork = {
  rpc_url: "http://127.0.0.1:8545/",
  payment_token_address: `0x${"11".repeat(20)}`,
  payment_vault_address: `0x${"22".repeat(20)}`,
};

// Reset for each test. Only browser-owned RTC objects are mocked; all node
// responses use the shared Rust wire contract and real session encryption.
export function mockWebRtc(nodes = [{}]) {
  const connections = [];
  const requests = [];
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
      requests.push({ node: this.index, method });
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
      this.channel.server = new BrowserTestNode(
        seed,
        this.channel.options.alreadyStored ?? false,
      );
      this.channel.server.set_chunk(this.channel.options.chunk ?? new Uint8Array());
      setTimeout(() => this.channel.onopen?.({}), 0);
    }
    close() {
      this.closed = true;
    }
  };
  return { endpoints, connections, requests };
}
