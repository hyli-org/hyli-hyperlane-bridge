import { defineChain } from 'viem'
import { sepolia } from 'viem/chains'
import { HYLI_RPC_URL } from '@/lib/hyperlane'

export const hyli = defineChain({
  id: 1337,
  name: 'Hyli',
  nativeCurrency: { name: 'Ether', symbol: 'ETH', decimals: 18 },
  rpcUrls: { default: { http: [HYLI_RPC_URL] } },
  testnet: true,
})

export { sepolia }
