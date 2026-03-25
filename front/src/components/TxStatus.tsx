'use client'

import type { BridgeStatus, Direction } from '@/hooks/useBridge'
import { ETHERSCAN_BASE_URL, HYLI_EXPLORER_BASE_URL } from '@/lib/hyperlane'

interface TxStatusProps {
  status: BridgeStatus
  direction: Direction
  onReset: () => void
}

function Spinner() {
  return (
    <svg className="animate-spin h-4 w-4 shrink-0" viewBox="0 0 24 24" fill="none">
      <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
      <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8v4a4 4 0 00-4 4H4z" />
    </svg>
  )
}

function CheckIcon() {
  return (
    <svg className="h-4 w-4 shrink-0 text-green-400" viewBox="0 0 20 20" fill="currentColor">
      <path fillRule="evenodd" d="M16.707 5.293a1 1 0 010 1.414l-8 8a1 1 0 01-1.414 0l-4-4a1 1 0 011.414-1.414L8 12.586l7.293-7.293a1 1 0 011.414 0z" clipRule="evenodd" />
    </svg>
  )
}

function XIcon() {
  return (
    <svg className="h-4 w-4 shrink-0 text-red-400" viewBox="0 0 20 20" fill="currentColor">
      <path fillRule="evenodd" d="M4.293 4.293a1 1 0 011.414 0L10 8.586l4.293-4.293a1 1 0 111.414 1.414L11.414 10l4.293 4.293a1 1 0 01-1.414 1.414L10 11.414l-4.293 4.293a1 1 0 01-1.414-1.414L8.586 10 4.293 5.707a1 1 0 010-1.414z" clipRule="evenodd" />
    </svg>
  )
}

function PendingCircle() {
  return <div className="h-4 w-4 shrink-0 rounded-full border border-current opacity-30" />
}

function StepRow({ done, active, failed, label, href, hash }: {
  done: boolean; active: boolean; failed: boolean
  label: string; href?: string; hash?: string
}) {
  return (
    <div className="space-y-1">
      <div className="flex items-center gap-2 text-xs">
        {done && !failed && <CheckIcon />}
        {failed && <XIcon />}
        {active && <Spinner />}
        {!done && !active && !failed && <PendingCircle />}
        <span className={active ? 'font-medium' : done && !failed ? 'opacity-60' : ''}>{label}</span>
      </div>
      {hash && href && (
        <a href={href} target="_blank" rel="noopener noreferrer"
          className="block ml-6 underline hover:opacity-80 font-mono text-xs break-all opacity-70">
          {hash}
        </a>
      )}
      {hash && !href && (
        <span className="block ml-6 font-mono text-xs break-all opacity-70">{hash}</span>
      )}
    </div>
  )
}

export function TxStatus({ status, direction, onReset }: TxStatusProps) {
  if (status.type === 'idle') return null

  const toHyli = direction === 'to_hyli'
  const srcExplorer = toHyli ? ETHERSCAN_BASE_URL : HYLI_EXPLORER_BASE_URL
  const dstExplorer = toHyli ? HYLI_EXPLORER_BASE_URL : ETHERSCAN_BASE_URL
  const srcChain = toHyli ? 'Sepolia' : 'Hyli'
  const dstChain = toHyli ? 'Hyli' : 'Sepolia'

  // For Hyli→Sepolia, use the Hyli blob tx hash for the explorer link (not the EVM hash).
  const srcDisplayHash = (status as { txHash?: string; hyliSrcHash?: string }).hyliSrcHash
    ?? (status as { txHash?: string }).txHash
  const srcExplorerHref = srcDisplayHash ? `${srcExplorer}/tx/${srcDisplayHash}` : undefined

  if (status.type === 'switching_chain') {
    return (
      <div className="p-4 rounded-xl bg-yellow-900/30 border border-yellow-700 text-yellow-300 text-sm">
        Switching to {srcChain} network&hellip;
      </div>
    )
  }

  if (status.type === 'pending') {
    return (
      <div className="p-4 rounded-xl bg-blue-900/30 border border-blue-700 text-blue-300 text-sm flex items-center gap-3">
        <Spinner />
        Confirm the transaction in your wallet&hellip;
      </div>
    )
  }

  if (status.type === 'confirming') {
    return (
      <div className="p-4 rounded-xl bg-blue-900/30 border border-blue-700 text-blue-300 text-sm space-y-3">
        <StepRow done={false} active={true} failed={false}
          label={`Bridge out \u2014 waiting for ${srcChain} confirmation\u2026`}
          hash={srcDisplayHash} href={srcExplorerHref} />
        <StepRow done={false} active={false} failed={false} label={`Bridge in \u2014 ${dstChain} relay pending`} />
      </div>
    )
  }

  if (status.type === 'source_reverted') {
    return (
      <div className="p-4 rounded-xl bg-red-900/30 border border-red-700 text-red-300 text-sm space-y-3">
        <StepRow done={false} active={false} failed={true}
          label={`Bridge out \u2014 ${srcChain} transaction reverted`}
          hash={srcDisplayHash} href={srcExplorerHref} />
        <button onClick={onReset} className="text-xs underline text-red-400 hover:text-red-300">Try again</button>
      </div>
    )
  }

  if (status.type === 'relaying') {
    return (
      <div className="p-4 rounded-xl bg-blue-900/30 border border-blue-700 text-blue-300 text-sm space-y-3">
        <StepRow done={true} active={false} failed={false}
          label={`Bridge out \u2014 confirmed on ${srcChain}`}
          hash={srcDisplayHash} href={srcExplorerHref} />
        <StepRow done={false} active={true} failed={false}
          label={status.destTxHash ? `Bridge in \u2014 confirming on ${dstChain}\u2026` : `Bridge in \u2014 waiting for ${dstChain} relay\u2026`}
          hash={status.destTxHash}
          href={status.destTxHash ? `${dstExplorer}/tx/${status.destTxHash}` : undefined} />
      </div>
    )
  }

  if (status.type === 'success') {
    return (
      <div className="p-4 rounded-xl bg-green-900/30 border border-green-700 text-green-300 text-sm space-y-3">
        <p className="font-semibold text-green-200">Bridge complete!</p>
        <StepRow done={true} active={false} failed={false}
          label={`Bridge out \u2014 confirmed on ${srcChain}`}
          hash={srcDisplayHash} href={srcExplorerHref} />
        <StepRow done={true} active={false} failed={false}
          label={`Bridge in \u2014 delivered on ${dstChain}`}
          hash={status.destTxHash}
          href={status.destTxHash ? `${dstExplorer}/tx/${status.destTxHash}` : undefined} />
        <button onClick={onReset} className="text-xs underline text-green-400 hover:text-green-300">Bridge again</button>
      </div>
    )
  }

  if (status.type === 'timeout') {
    return (
      <div className="p-4 rounded-xl bg-yellow-900/30 border border-yellow-700 text-yellow-300 text-sm space-y-3">
        <p className="font-semibold">Relay is taking longer than expected</p>
        <StepRow done={true} active={false} failed={false}
          label={`Bridge out \u2014 confirmed on ${srcChain}`}
          hash={srcDisplayHash} href={srcExplorerHref} />
        <StepRow done={false} active={false} failed={false}
          label={`Bridge in \u2014 ${dstChain} relay pending\u2026`}
          hash={status.destTxHash}
          href={status.destTxHash ? `${dstExplorer}/tx/${status.destTxHash}` : undefined} />
        <p className="text-yellow-400/80 text-xs">The relayer may still deliver your funds. Check your {dstChain} balance later.</p>
        <button onClick={onReset} className="text-xs underline text-yellow-400 hover:text-yellow-300">Dismiss</button>
      </div>
    )
  }

  if (status.type === 'error') {
    return (
      <div className="p-4 rounded-xl bg-red-900/30 border border-red-700 text-red-300 text-sm space-y-2">
        <p className="font-semibold">Error</p>
        <p className="text-xs break-all">{status.message}</p>
        <button onClick={onReset} className="text-xs underline text-red-400 hover:text-red-300">Try again</button>
      </div>
    )
  }

  return null
}
