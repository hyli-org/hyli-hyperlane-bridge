import { defineChain } from 'viem'
import { sepolia } from 'viem/chains'
import { DEFAULT_HYLI_RPC_URL } from '@/lib/runtimeConfig'

export function createHyliChain(rpcUrl = DEFAULT_HYLI_RPC_URL) {
  return defineChain({
    id: 1213811785,
    name: 'Hyli',
    nativeCurrency: { name: 'Ether', symbol: 'ETH', decimals: 18 },
    rpcUrls: { default: { http: [rpcUrl] } },
    testnet: true,
  })
}

export const hyli = createHyliChain()

export { sepolia }
