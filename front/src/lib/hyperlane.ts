import { addressToBytes32 } from '@hyperlane-xyz/utils'

export const SEPOLIA_WARP_CONTRACT = '0x64E11a41aa0E0d65519fC7B8544ca0d2bD8d0eEF' as const
export const SEPOLIA_MAILBOX = '0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766' as const
export const HYLI_WARP_CONTRACT = '0x22CE25BFa5Dcd58A3B52c2A5fa262bDF079A5456' as const
export const HYLI_MAILBOX = '0x0f00856CbD2D29a73d673cDCA101EBfa7083C5C1' as const
// ERC-20 token representing bridged ETH on Hyli's EVM (override via NEXT_PUBLIC_HYLI_TOKEN_CONTRACT)
export const HYLI_TOKEN_CONTRACT =
  (process.env.NEXT_PUBLIC_HYLI_TOKEN_CONTRACT ?? '0x22CE25BFa5Dcd58A3B52c2A5fa262bDF079A5456') as `0x${string}`
export const HYLI_DOMAIN = 1337
export const HYLI_CHAIN_ID = 1337
export const SEPOLIA_DOMAIN = 11155111
export const SEPOLIA_CHAIN_ID = 11155111
export const HYLI_RPC_URL =
  process.env.NEXT_PUBLIC_HYLI_RPC_URL ?? 'http://localhost:9002/rpc'
export const HYLI_INDEXER_URL =
  process.env.NEXT_PUBLIC_HYLI_INDEXER_URL ?? 'http://localhost:4322'

// Explorer base URLs (override via env vars)
export const ETHERSCAN_BASE_URL =
  process.env.NEXT_PUBLIC_ETHERSCAN_BASE_URL ?? 'https://sepolia.etherscan.io'
export const HYLI_EXPLORER_BASE_URL =
  process.env.NEXT_PUBLIC_HYLI_EXPLORER_BASE_URL ?? 'https://explorer.hyli.dev'

export const TRANSFER_REMOTE_ABI = [
  {
    name: 'transferRemote',
    type: 'function',
    stateMutability: 'payable',
    inputs: [
      { name: 'destination', type: 'uint32' },
      { name: 'recipient', type: 'bytes32' },
      { name: 'amount', type: 'uint256' },
    ],
    outputs: [{ name: 'messageId', type: 'bytes32' }],
  },
] as const

export const QUOTE_GAS_PAYMENT_ABI = [
  {
    name: 'quoteGasPayment',
    type: 'function',
    stateMutability: 'view',
    inputs: [{ name: 'destinationDomain', type: 'uint32' }],
    outputs: [{ name: 'fee', type: 'uint256' }],
  },
] as const

export const PROCESS_ID_ABI = [
  {
    name: 'ProcessId',
    type: 'event',
    inputs: [{ name: 'messageId', type: 'bytes32', indexed: true }],
  },
] as const

export function encodeRecipient(address: `0x${string}`): `0x${string}` {
  return addressToBytes32(address) as `0x${string}`
}
