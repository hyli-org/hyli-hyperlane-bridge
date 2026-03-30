'use client'

import { useAccount } from 'wagmi'
import { SEPOLIA_CHAIN_ID, HYLI_CHAIN_ID } from '@/lib/hyperlane'
import type { BridgeStatus, Direction } from '@/hooks/useBridge'

interface BridgeButtonProps {
  amount: string
  status: BridgeStatus
  direction: Direction
  insufficientFunds: boolean
  rpcError: string | null
  onBridge: () => void
}

export function BridgeButton({ amount, status, direction, insufficientFunds, rpcError, onBridge }: BridgeButtonProps) {
  const { isConnected, chainId } = useAccount()
  const toHyli = direction === 'to_hyli'
  const sourceChainId = toHyli ? SEPOLIA_CHAIN_ID : HYLI_CHAIN_ID
  const srcChain = toHyli ? 'Sepolia' : 'Hyli'
  const dstChain = toHyli ? 'Hyli' : 'Sepolia'

  const isLoading =
    status.type === 'pending' ||
    status.type === 'switching_chain' ||
    status.type === 'confirming' ||
    status.type === 'relaying'

  function getLabel() {
    if (!isConnected) return 'Connect Wallet'
    if (status.type === 'switching_chain') return `Switching to ${srcChain}...`
    if (chainId !== sourceChainId) return `Switch to ${srcChain}`
    if (status.type === 'pending') return 'Confirm in wallet...'
    if (status.type === 'confirming') return `Confirming on ${srcChain}...`
    if (status.type === 'relaying') return `Relaying to ${dstChain}...`
    if (rpcError) return 'Hyli RPC unavailable'
    if (!amount || parseFloat(amount) <= 0) return 'Enter amount'
    if (insufficientFunds) return 'Insufficient balance'
    return `Bridge to ${dstChain}`
  }

  return (
    <button
      onClick={onBridge}
      disabled={!isConnected || isLoading || !!rpcError || !amount || parseFloat(amount) <= 0 || insufficientFunds}
      className="w-full py-3.5 rounded-xl font-semibold text-white transition-all
        bg-gradient-to-r from-blue-600 to-violet-600
        hover:from-blue-500 hover:to-violet-500
        disabled:opacity-50 disabled:cursor-not-allowed
        focus:outline-none focus:ring-2 focus:ring-blue-500"
    >
      {getLabel()}
    </button>
  )
}
