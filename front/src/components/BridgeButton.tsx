'use client'

import { useAccount } from 'wagmi'
import { SEPOLIA_CHAIN_ID } from '@/lib/hyperlane'
import type { BridgeStatus } from '@/hooks/useBridge'

interface BridgeButtonProps {
  amount: string
  status: BridgeStatus
  onBridge: () => void
}

export function BridgeButton({ amount, status, onBridge }: BridgeButtonProps) {
  const { isConnected, chainId } = useAccount()

  const isLoading =
    status.type === 'pending' || status.type === 'switching_chain'

  const isOnSepolia = chainId === SEPOLIA_CHAIN_ID

  function getLabel() {
    if (!isConnected) return 'Connect Wallet'
    if (status.type === 'switching_chain') return 'Switching to Sepolia…'
    if (!isOnSepolia) return 'Switch to Sepolia'
    if (status.type === 'pending') return 'Bridging…'
    if (!amount || parseFloat(amount) <= 0) return 'Enter amount'
    return 'Bridge to Hyli'
  }

  const disabled =
    !isConnected ||
    isLoading ||
    !amount ||
    parseFloat(amount) <= 0

  return (
    <button
      onClick={onBridge}
      disabled={disabled}
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
