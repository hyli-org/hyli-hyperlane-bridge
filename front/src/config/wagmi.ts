import { createConfig, http } from 'wagmi'
import { sepolia, hyli } from './chains'

export const wagmiConfig = createConfig({
  chains: [sepolia, hyli],
  multiInjectedProviderDiscovery: true,
  transports: {
    [sepolia.id]: http(),
    [hyli.id]: http(hyli.rpcUrls.default.http[0]),
  },
})
