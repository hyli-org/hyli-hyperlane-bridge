import { addressToBytes32 } from '@hyperlane-xyz/utils'

export const SEPOLIA_WARP_CONTRACT = '0x64E11a41aa0E0d65519fC7B8544ca0d2bD8d0eEF' as const
export const HYLI_DOMAIN = 1337
export const SEPOLIA_CHAIN_ID = 11155111

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

export function encodeRecipient(address: `0x${string}`): `0x${string}` {
  return addressToBytes32(address) as `0x${string}`
}
