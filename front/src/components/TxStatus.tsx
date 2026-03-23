'use client'

import type { BridgeStatus } from '@/hooks/useBridge'

interface TxStatusProps {
  status: BridgeStatus
  onReset: () => void
}

export function TxStatus({ status, onReset }: TxStatusProps) {
  if (status.type === 'idle') return null

  if (status.type === 'switching_chain') {
    return (
      <div className="p-4 rounded-xl bg-yellow-900/30 border border-yellow-700 text-yellow-300 text-sm">
        Switching to Sepolia network…
      </div>
    )
  }

  if (status.type === 'pending') {
    return (
      <div className="p-4 rounded-xl bg-blue-900/30 border border-blue-700 text-blue-300 text-sm flex items-center gap-3">
        <svg className="animate-spin h-4 w-4 shrink-0" viewBox="0 0 24 24" fill="none">
          <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
          <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8v4a4 4 0 00-4 4H4z" />
        </svg>
        Waiting for Sepolia confirmation…
      </div>
    )
  }

  if (status.type === 'success') {
    return (
      <div className="p-4 rounded-xl bg-green-900/30 border border-green-700 text-green-300 text-sm space-y-3">
        <p className="font-semibold">Transaction submitted!</p>
        <p>
          <a
            href={`https://sepolia.etherscan.io/tx/${status.txHash}`}
            target="_blank"
            rel="noopener noreferrer"
            className="underline hover:text-green-200 font-mono text-xs break-all"
          >
            {status.txHash}
          </a>
        </p>
        <p className="text-green-400/80 text-xs">
          Waiting for Hyli relay… Your balance will appear on the Hyli RPC once the relayer delivers the message.
        </p>
        <button
          onClick={onReset}
          className="text-xs underline text-green-400 hover:text-green-300"
        >
          Bridge again
        </button>
      </div>
    )
  }

  if (status.type === 'error') {
    return (
      <div className="p-4 rounded-xl bg-red-900/30 border border-red-700 text-red-300 text-sm space-y-2">
        <p className="font-semibold">Error</p>
        <p className="text-xs break-all">{status.message}</p>
        <button
          onClick={onReset}
          className="text-xs underline text-red-400 hover:text-red-300"
        >
          Try again
        </button>
      </div>
    )
  }

  return null
}
