import assert from "node:assert/strict";
import test from "node:test";
import { blake3 } from "@noble/hashes/blake3.js";
import { ml_dsa65 } from "@noble/post-quantum/ml-dsa.js";
import { encode } from "@msgpack/msgpack";
import { keccak256 } from "ethers";
import { bytesToHex } from "./protocol.js";
import { paymentQuoteHash, verifyStorageQuote } from "./payment.js";

const BASELINE_PRICE = 3_906_250_000_000_000n;
const PRICE_COEFFICIENT = 35_156_250_000_000_000n;
const COMMITMENT_CONTEXT = new TextEncoder().encode(
  "autonomi.ant.replication.storage_commitment.v1",
);
const COMMITMENT_HASH_DOMAIN = new TextEncoder().encode(
  "autonomi.ant.replication.commitment_hash.v1",
);

function concatBytes(...parts) {
  const output = new Uint8Array(parts.reduce((total, part) => total + part.length, 0));
  let offset = 0;
  for (const part of parts) {
    output.set(part, offset);
    offset += part.length;
  }
  return output;
}

function littleEndian(value, length) {
  let remaining = BigInt(value);
  const result = new Uint8Array(length);
  for (let index = 0; index < length; index += 1) {
    result[index] = Number(remaining & 0xffn);
    remaining >>= 8n;
  }
  return result;
}

function postcardVarint(value) {
  let remaining = BigInt(value);
  const bytes = [];
  do {
    let byte = Number(remaining & 0x7fn);
    remaining >>= 7n;
    if (remaining > 0n) byte |= 0x80;
    bytes.push(byte);
  } while (remaining > 0n);
  return Uint8Array.from(bytes);
}

function signedBaselineQuote() {
  const content = Uint8Array.from({ length: 32 }, (_, index) => index);
  const rewards = new Uint8Array(20).fill(0x44);
  const timestamp = 1_775_000_000n;
  const { publicKey, secretKey } = ml_dsa65.keygen(new Uint8Array(32).fill(0x17));
  const payload = concatBytes(
    content,
    littleEndian(timestamp, 8),
    littleEndian(BASELINE_PRICE, 32),
    rewards,
    littleEndian(0n, 4),
    Uint8Array.of(0),
  );
  const signature = ml_dsa65.sign(payload, secretKey);
  const quoteHash = keccak256(concatBytes(payload, publicKey, signature)).slice(2);
  const peerId = bytesToHex(blake3(publicKey));
  return {
    peerId,
    address: bytesToHex(content),
    quote: {
      peer_id: peerId,
      content: bytesToHex(content),
      timestamp_secs: Number(timestamp),
      price: BASELINE_PRICE.toString(),
      rewards_address: bytesToHex(rewards),
      public_key: bytesToHex(publicKey),
      signature: bytesToHex(signature),
      committed_key_count: 0,
      commitment_pin: null,
      quote_hash: quoteHash,
      commitment: null,
    },
  };
}

function signedBoundQuote() {
  const content = new Uint8Array(32).fill(0x31);
  const rewards = new Uint8Array(20).fill(0x42);
  const root = new Uint8Array(32).fill(0x53);
  const keyCount = 23;
  const timestamp = 1_775_000_001n;
  const { publicKey, secretKey } = ml_dsa65.keygen(new Uint8Array(32).fill(0x29));
  const peerId = blake3(publicKey);
  const commitmentPayload = concatBytes(
    root,
    littleEndian(keyCount, 4),
    peerId,
    littleEndian(publicKey.length, 4),
    publicKey,
  );
  const commitmentSignature = ml_dsa65.sign(commitmentPayload, secretKey, {
    context: COMMITMENT_CONTEXT,
  });
  const postcard = concatBytes(
    root,
    postcardVarint(keyCount),
    peerId,
    postcardVarint(publicKey.length),
    publicKey,
    postcardVarint(commitmentSignature.length),
    commitmentSignature,
  );
  const pin = blake3(concatBytes(COMMITMENT_HASH_DOMAIN, postcard));
  const price =
    BASELINE_PRICE +
    (BigInt(keyCount) * BigInt(keyCount) * PRICE_COEFFICIENT) / (6_000n * 6_000n);
  const quotePayload = concatBytes(
    content,
    littleEndian(timestamp, 8),
    littleEndian(price, 32),
    rewards,
    littleEndian(keyCount, 4),
    Uint8Array.of(1),
    pin,
  );
  const quoteSignature = ml_dsa65.sign(quotePayload, secretKey);
  const quoteHash = keccak256(
    concatBytes(quotePayload, publicKey, quoteSignature),
  ).slice(2);
  const encodedCommitment = encode([
    Array.from(root),
    keyCount,
    Array.from(peerId),
    Array.from(publicKey),
    Array.from(commitmentSignature),
  ]);
  const commitment = {
    encoded: bytesToHex(encodedCommitment),
    root: bytesToHex(root),
    key_count: keyCount,
    sender_peer_id: bytesToHex(peerId),
    sender_public_key: bytesToHex(publicKey),
    signature: bytesToHex(commitmentSignature),
  };
  return {
    address: bytesToHex(content),
    peerId: bytesToHex(peerId),
    quote: {
      peer_id: bytesToHex(peerId),
      content: bytesToHex(content),
      timestamp_secs: Number(timestamp),
      price: price.toString(),
      rewards_address: bytesToHex(rewards),
      public_key: bytesToHex(publicKey),
      signature: bytesToHex(quoteSignature),
      committed_key_count: keyCount,
      commitment_pin: bytesToHex(pin),
      quote_hash: quoteHash,
      commitment,
    },
  };
}

test("accepts a correctly bound and signed native-shaped storage quote", () => {
  const { quote, address, peerId } = signedBaselineQuote();
  const verified = verifyStorageQuote(quote, address, peerId);
  assert.equal(verified.quoteHash, quote.quote_hash);
  assert.equal(verified.amount, BASELINE_PRICE * 3n);
  assert.equal(verified.rewardsAddress, `0x${quote.rewards_address}`);
});

test("uses evmlib's Keccak-256 PaymentQuote hash", () => {
  // Fixed native hash-vector input split as bytes-for-signing, public key,
  // and signature. PaymentQuote::hash() concatenates these exact byte slices.
  assert.equal(
    paymentQuoteHash(Uint8Array.of(0, 1), Uint8Array.of(2), Uint8Array.of(3)),
    "d98f2e8134922f73748703c8e7084d42f13d2fa1439936ef5a3abcf5646fe83f",
  );
});

test("rejects quote field tampering before any payment", () => {
  const fixture = signedBaselineQuote();
  assert.throws(
    () =>
      verifyStorageQuote(
        { ...fixture.quote, price: (BASELINE_PRICE + 1n).toString() },
        fixture.address,
        fixture.peerId,
      ),
    /invalid ML-DSA-65 signature|price/,
  );
  assert.throws(
    () => verifyStorageQuote(fixture.quote, fixture.address, "ff".repeat(32)),
    /different WebRtcDirect peer/,
  );
  const signature = Uint8Array.from(Buffer.from(fixture.quote.signature, "hex"));
  signature[100] ^= 1;
  assert.throws(
    () =>
      verifyStorageQuote(
        { ...fixture.quote, signature: bytesToHex(signature) },
        fixture.address,
        fixture.peerId,
      ),
    /invalid ML-DSA-65 signature/,
  );
});

test("verifies a bound commitment and the exact native sidecar before payment", () => {
  const fixture = signedBoundQuote();
  assert.doesNotThrow(() =>
    verifyStorageQuote(fixture.quote, fixture.address, fixture.peerId),
  );

  const decodedShape = [
    Array(32).fill(0x99),
    fixture.quote.committed_key_count,
    Array.from(Buffer.from(fixture.quote.commitment.sender_peer_id, "hex")),
    Array.from(Buffer.from(fixture.quote.commitment.sender_public_key, "hex")),
    Array.from(Buffer.from(fixture.quote.commitment.signature, "hex")),
  ];
  const mismatchedSidecar = {
    ...fixture.quote.commitment,
    encoded: bytesToHex(encode(decodedShape)),
  };
  assert.throws(
    () =>
      verifyStorageQuote(
        { ...fixture.quote, commitment: mismatchedSidecar },
        fixture.address,
        fixture.peerId,
      ),
    /sidecar differs/,
  );
});
