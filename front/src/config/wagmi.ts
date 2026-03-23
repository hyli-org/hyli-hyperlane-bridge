import { createConfig, http } from 'wagmi'
import { sepolia, hyli } from './chains'

export const wagmiConfig = createConfig({
  chains: [sepolia, hyli],
  multiInjectedProviderDiscovery: true,
  transports: {
    [sepolia.id]: http(),
    [hyli.id]: http('http://localhost:4000/rpc'),
  },
})
