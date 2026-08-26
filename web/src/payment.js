import {
  Contract,
  JsonRpcProvider,
  MaxUint256,
  NonceManager,
  Wallet,
} from "ethers";

const TOKEN_ABI = [
  "function allowance(address owner, address spender) view returns (uint256)",
  "function approve(address spender, uint256 amount) returns (bool)",
];
const VAULT_ABI = [
  "function payForQuotes((address rewardsAddress,uint256 amount,bytes32 quoteHash)[] payments)",
];

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
  const totalAmount = verifiedQuotes.reduce(
    (total, quote) => total + BigInt(quote.amount),
    0n,
  );
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
  return {
    transactionHash: transaction.hash,
    walletAddress: wallet.address,
    totalAmount: totalAmount.toString(),
  };
}
