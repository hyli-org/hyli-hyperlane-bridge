import { defineChain } from 'viem'
import { sepolia } from 'viem/chains'

export const hyli = defineChain({
  id: 1337,
  name: 'Hyli',
  nativeCurrency: { name: 'Ether', symbol: 'ETH', decimals: 18 },
  rpcUrls: { default: { http: ['http://localhost:4000/rpc'] } },
  testnet: true,
})

export { sepolia }
