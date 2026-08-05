import { ml_dsa65 } from "@noble/post-quantum/ml-dsa.js";
import { decode } from "@msgpack/msgpack";
import {
  Contract,
  JsonRpcProvider,
  keccak256,
  MaxUint256,
  NonceManager,
  Wallet,
} from "ethers";
import { contentAddress as contentAddressNative } from "../pkg/ant_core.js";
import { bytesToHex, hexToBytes } from "./protocol.js";

const U256_MAX = (1n << 256n) - 1n;
const PAYMENT_MULTIPLIER = 3n;
const PRICE_BASELINE_WEI = 3_906_250_000_000_000n;
const PRICE_COEFFICIENT_WEI = 35_156_250_000_000_000n;
const DIVISOR_SQUARED = 6_000n * 6_000n;
const MAX_COMMITMENT_KEY_COUNT = 1_000_000;
const MAX_COMMITMENT_SIDECAR_BYTES = 8 * 1024;
const DOMAIN_COMMITMENT = new TextEncoder().encode(
  "autonomi.ant.replication.storage_commitment.v1",
);
const DOMAIN_COMMITMENT_HASH = new TextEncoder().encode(
  "autonomi.ant.replication.commitment_hash.v1",
);

const TOKEN_ABI = [
  "function allowance(address owner, address spender) view returns (uint256)",
  "function approve(address spender, uint256 amount) returns (bool)",
];
const VAULT_ABI = [
  "function payForQuotes((address rewardsAddress,uint256 amount,bytes32 quoteHash)[] payments)",
];

function concatBytes(...parts) {
  const size = parts.reduce((total, part) => total + part.length, 0);
  const output = new Uint8Array(size);
  let offset = 0;
  for (const part of parts) {
    output.set(part, offset);
    offset += part.length;
  }
  return output;
}

function unsignedLittleEndian(value, length) {
  let remaining = BigInt(value);
  if (remaining < 0n || remaining >= 1n << BigInt(length * 8)) {
    throw new Error(`Unsigned integer does not fit ${length} bytes`);
  }
  const result = new Uint8Array(length);
  for (let index = 0; index < length; index += 1) {
    result[index] = Number(remaining & 0xffn);
    remaining >>= 8n;
  }
  return result;
}

function postcardVarint(value) {
  let remaining = BigInt(value);
  const result = [];
  do {
    let byte = Number(remaining & 0x7fn);
    remaining >>= 7n;
    if (remaining > 0n) byte |= 0x80;
    result.push(byte);
  } while (remaining > 0n);
  return Uint8Array.from(result);
}

function canonicalQuoteBytes(quote) {
  const content = hexToBytes(quote.content, 32);
  const rewardsAddress = hexToBytes(quote.rewards_address, 20);
  const price = parseAmount(quote.price, "quote price");
  const timestamp = parseAmount(String(quote.timestamp_secs), "quote timestamp");
  const count = quote.committed_key_count;
  if (!Number.isSafeInteger(count) || count < 0 || count > MAX_COMMITMENT_KEY_COUNT) {
    throw new Error(`Invalid committed key count ${count}`);
  }
  let pin = Uint8Array.of(0);
  if (quote.commitment_pin !== null && quote.commitment_pin !== undefined) {
    pin = concatBytes(Uint8Array.of(1), hexToBytes(quote.commitment_pin, 32));
  }
  return concatBytes(
    content,
    unsignedLittleEndian(timestamp, 8),
    unsignedLittleEndian(price, 32),
    rewardsAddress,
    unsignedLittleEndian(BigInt(count), 4),
    pin,
  );
}

export function paymentQuoteHash(signedBytes, publicKey, signature) {
  // This is the EVM-facing PaymentQuote hash settled by the payment vault.
  // Native evmlib deliberately uses Keccak-256 here, while ANT identities,
  // chunk addresses, and commitment pins use BLAKE3.
  return keccak256(concatBytes(signedBytes, publicKey, signature)).slice(2);
}

function parseAmount(value, label) {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/.test(value)) {
    throw new Error(`Invalid ${label}`);
  }
  const amount = BigInt(value);
  if (amount > U256_MAX) throw new Error(`${label} exceeds uint256`);
  return amount;
}

function calculatePrice(keyCount) {
  const count = BigInt(keyCount);
  return PRICE_BASELINE_WEI + (count * count * PRICE_COEFFICIENT_WEI) / DIVISOR_SQUARED;
}

function equalBytes(left, right) {
  if (left.length !== right.length) return false;
  return left.every((byte, index) => byte === right[index]);
}

function verifyEncodedCommitment(commitment, normalized) {
  const encoded = hexToBytes(commitment.encoded);
  if (encoded.length > MAX_COMMITMENT_SIDECAR_BYTES) {
    throw new Error("Storage commitment sidecar exceeds the protocol limit");
  }
  let fields;
  try {
    fields = decode(encoded);
  } catch (error) {
    throw new Error(`Storage commitment sidecar is not valid MessagePack: ${error.message}`, {
      cause: error,
    });
  }
  if (!Array.isArray(fields) || fields.length !== 5) {
    throw new Error("Storage commitment sidecar has an invalid native shape");
  }
  const [root, keyCount, peerId, publicKey, signature] = fields;
  if (
    keyCount !== normalized.keyCount ||
    !equalBytes(fixedCommitmentBytes(root, 32), normalized.root) ||
    !equalBytes(fixedCommitmentBytes(peerId, 32), normalized.peerId) ||
    !equalBytes(fixedCommitmentBytes(publicKey), normalized.publicKey) ||
    !equalBytes(fixedCommitmentBytes(signature), normalized.signature)
  ) {
    throw new Error("Storage commitment sidecar differs from the verified commitment");
  }
}

function fixedCommitmentBytes(value, length) {
  const bytes = value instanceof Uint8Array ? value : Uint8Array.from(value ?? []);
  if (length !== undefined && bytes.length !== length) {
    throw new Error(`Storage commitment field must contain ${length} bytes`);
  }
  return bytes;
}

function verifyCommitment(commitment, quote) {
  if (!commitment || typeof commitment !== "object") {
    throw new Error("Bound quote omitted its storage commitment");
  }
  const root = hexToBytes(commitment.root, 32);
  const peerId = hexToBytes(commitment.sender_peer_id, 32);
  const publicKey = hexToBytes(commitment.sender_public_key);
  const signature = hexToBytes(commitment.signature);
  const keyCount = commitment.key_count;
  if (
    publicKey.length !== ml_dsa65.lengths.publicKey ||
    signature.length !== ml_dsa65.lengths.signature
  ) {
    throw new Error("Storage commitment has invalid ML-DSA-65 field lengths");
  }
  if (!Number.isSafeInteger(keyCount) || keyCount !== quote.committed_key_count) {
    throw new Error("Storage commitment key count does not match quote");
  }
  if (contentAddressNative(publicKey) !== quote.peer_id.toLowerCase()) {
    throw new Error("Storage commitment public key is not bound to quote peer");
  }
  if (bytesToHex(peerId) !== quote.peer_id.toLowerCase()) {
    throw new Error("Storage commitment belongs to a different peer");
  }
  verifyEncodedCommitment(commitment, {
    root,
    keyCount,
    peerId,
    publicKey,
    signature,
  });
  const signedPayload = concatBytes(
    root,
    unsignedLittleEndian(BigInt(keyCount), 4),
    peerId,
    unsignedLittleEndian(BigInt(publicKey.length), 4),
    publicKey,
  );
  if (
    !ml_dsa65.verify(signature, signedPayload, publicKey, {
      context: DOMAIN_COMMITMENT,
    })
  ) {
    throw new Error("Storage commitment has an invalid ML-DSA-65 signature");
  }

  const postcard = concatBytes(
    root,
    postcardVarint(keyCount),
    peerId,
    postcardVarint(publicKey.length),
    publicKey,
    postcardVarint(signature.length),
    signature,
  );
  const pin = contentAddressNative(concatBytes(DOMAIN_COMMITMENT_HASH, postcard));
  if (pin !== quote.commitment_pin.toLowerCase()) {
    throw new Error("Storage commitment does not resolve the quote pin");
  }
  return true;
}

export function verifyStorageQuote(quote, expectedAddress, expectedPeerId) {
  if (!quote || typeof quote !== "object") throw new Error("Node returned no quote");
  hexToBytes(expectedAddress, 32);
  hexToBytes(expectedPeerId, 32);
  if (quote.content?.toLowerCase() !== expectedAddress.toLowerCase()) {
    throw new Error("Storage quote is for a different chunk");
  }
  if (quote.peer_id?.toLowerCase() !== expectedPeerId.toLowerCase()) {
    throw new Error("Storage quote belongs to a different WebTransport peer");
  }
  const publicKey = hexToBytes(quote.public_key);
  const signature = hexToBytes(quote.signature);
  if (publicKey.length !== ml_dsa65.lengths.publicKey) {
    throw new Error(`Storage quote has a ${publicKey.length}-byte public key`);
  }
  if (signature.length !== ml_dsa65.lengths.signature) {
    throw new Error(`Storage quote has a ${signature.length}-byte signature`);
  }
  if (contentAddressNative(publicKey) !== quote.peer_id.toLowerCase()) {
    throw new Error("Storage quote public key is not bound to its peer ID");
  }
  const signedBytes = canonicalQuoteBytes(quote);
  if (!ml_dsa65.verify(signature, signedBytes, publicKey)) {
    throw new Error("Storage quote has an invalid ML-DSA-65 signature");
  }
  const quoteHash = paymentQuoteHash(signedBytes, publicKey, signature);
  if (quoteHash !== quote.quote_hash?.toLowerCase()) {
    throw new Error("Storage quote hash does not match its signed fields");
  }
  const price = parseAmount(quote.price, "quote price");
  if (price !== calculatePrice(quote.committed_key_count)) {
    throw new Error("Storage quote price is not bound to its committed key count");
  }
  if (quote.committed_key_count === 0) {
    if (quote.commitment_pin !== null || quote.commitment !== null) {
      throw new Error("Baseline storage quote has an incoherent commitment");
    }
  } else {
    if (!quote.commitment_pin) throw new Error("Bound storage quote omitted its pin");
    verifyCommitment(quote.commitment, quote);
  }
  hexToBytes(quote.rewards_address, 20);
  return {
    quote,
    quoteHash,
    rewardsAddress: `0x${quote.rewards_address.replace(/^0x/i, "")}`,
    amount: price * PAYMENT_MULTIPLIER,
  };
}

export async function payForStorageQuotes(
  paymentNetwork,
  verifiedQuotes,
  walletSecret,
  { onProgress = () => {} } = {},
) {
  if (!Array.isArray(verifiedQuotes) || verifiedQuotes.length === 0) {
    return { transactionHash: undefined, walletAddress: undefined, totalAmount: 0n };
  }
  const provider = new JsonRpcProvider(paymentNetwork.rpc_url);
  let wallet;
  try {
    wallet = new Wallet(walletSecret, provider);
  } catch (error) {
    throw new Error("Wallet secret key is invalid", { cause: error });
  }
  const signer = new NonceManager(wallet);
  const totalAmount = verifiedQuotes.reduce((total, quote) => total + quote.amount, 0n);
  const token = new Contract(paymentNetwork.payment_token_address, TOKEN_ABI, signer);
  const vault = new Contract(paymentNetwork.payment_vault_address, VAULT_ABI, signer);
  const allowance = await token.allowance(wallet.address, paymentNetwork.payment_vault_address);
  if (allowance < totalAmount) {
    onProgress(`Approving the payment vault from wallet ${wallet.address}`);
    const approval = await token.approve(paymentNetwork.payment_vault_address, MaxUint256);
    await approval.wait();
  }
  const payments = verifiedQuotes.map((quote) => ({
    rewardsAddress: quote.rewardsAddress,
    amount: quote.amount,
    quoteHash: `0x${quote.quoteHash}`,
  }));
  onProgress(`Submitting one payment for ${payments.length} storage quote(s)`);
  const transaction = await vault.payForQuotes(payments);
  const receipt = await transaction.wait();
  if (!receipt || receipt.status !== 1) throw new Error("Storage payment transaction reverted");
  onProgress(`Payment confirmed in ${transaction.hash}`);
  return { transactionHash: transaction.hash, walletAddress: wallet.address, totalAmount };
}
