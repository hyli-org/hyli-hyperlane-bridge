'use client'

import { useBalance, useAccount } from 'wagmi'
import { SEPOLIA_CHAIN_ID } from '@/lib/hyperlane'

export function useNativeBalance() {
  const { address } = useAccount()
  const { data, isLoading, refetch } = useBalance({
    address,
    chainId: SEPOLIA_CHAIN_ID,
  })

  return {
    balance: data?.value ?? 0n,
    formatted: data?.formatted ?? '0',
    symbol: data?.symbol ?? 'ETH',
    isLoading,
    refetch,
  }
}
