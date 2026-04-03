'use client'

import { useBalance, useReadContracts, useAccount } from 'wagmi'
import { formatUnits } from 'viem'
import { SEPOLIA_CHAIN_ID, HYLI_CHAIN_ID } from '@/lib/hyperlane'
import { useRuntimeConfig } from '@/lib/runtimeConfig'

const ERC20_ABI = [
  {
    name: 'balanceOf',
    type: 'function',
    stateMutability: 'view',
    inputs: [{ name: 'account', type: 'address' }],
    outputs: [{ name: '', type: 'uint256' }],
  },
  {
    name: 'decimals',
    type: 'function',
    stateMutability: 'view',
    inputs: [],
    outputs: [{ name: '', type: 'uint8' }],
  },
] as const

export function useNativeBalance(chainId: number = SEPOLIA_CHAIN_ID) {
  const { address } = useAccount()
  const { hyliWarpContract } = useRuntimeConfig()
  const isHyli = chainId === HYLI_CHAIN_ID

  // On Hyli: 2 calls (balanceOf + decimals) instead of wagmi's 4-call useBalance(token) path
  const { data: tokenData, isLoading: tokenLoading, refetch: tokenRefetch } = useReadContracts({
    contracts: [
      {
        address: hyliWarpContract,
        abi: ERC20_ABI,
        functionName: 'balanceOf',
        args: [address!],
        chainId: HYLI_CHAIN_ID,
      },
      {
        address: hyliWarpContract,
        abi: ERC20_ABI,
        functionName: 'decimals',
        chainId: HYLI_CHAIN_ID,
      },
    ],
    query: { enabled: isHyli && !!address },
  })

  const { data: nativeData, isLoading: nativeLoading, refetch: nativeRefetch } = useBalance({
    address,
    chainId,
    query: { enabled: !isHyli && !!address },
  })

  if (isHyli) {
    const raw = (tokenData?.[0]?.result as bigint | undefined) ?? 0n
    const decimals = (tokenData?.[1]?.result as number | undefined) ?? 18
    return {
      balance: raw,
      formatted: formatUnits(raw, decimals),
      symbol: 'ETH',
      isLoading: tokenLoading,
      refetch: tokenRefetch,
    }
  }

  return {
    balance: nativeData?.value ?? 0n,
    formatted: nativeData?.formatted ?? '0',
    symbol: nativeData?.symbol ?? 'ETH',
    isLoading: nativeLoading,
    refetch: nativeRefetch,
  }
}
