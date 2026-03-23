'use client'

import { useState, useEffect, useRef } from 'react'
import { parseEther, keccak256, toBytes } from 'viem'
import { useWriteContract, useReadContract, useAccount, useSwitchChain, useWaitForTransactionReceipt } from 'wagmi'
import {
  SEPOLIA_WARP_CONTRACT,
  SEPOLIA_MAILBOX,
  HYLI_DOMAIN,
  SEPOLIA_CHAIN_ID,
  HYLI_RPC_URL,
  HYLI_INDEXER_URL,
  TRANSFER_REMOTE_ABI,
  QUOTE_GAS_PAYMENT_ABI,
  encodeRecipient,
} from '@/lib/hyperlane'

export type BridgeStatus =
  | { type: 'idle' }
  | { type: 'switching_chain' }
  | { type: 'pending' }
  | { type: 'confirming'; txHash: `0x${string}` }
  | { type: 'sepolia_reverted'; txHash: `0x${string}` }
  | { type: 'relaying'; txHash: `0x${string}`; hyliTxHash?: string }
  | { type: 'hyli_success'; txHash: `0x${string}`; hyliTxHash?: string }
  | { type: 'hyli_timeout'; txHash: `0x${string}`; hyliTxHash?: string }
  | { type: 'error'; message: string }

async function rpcCall(method: string, params: unknown[]): Promise<unknown> {
  const res = await fetch(HYLI_RPC_URL, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
  })
  const data = await res.json()
  if (data.error) console.warn(`[hyli rpc] ${method} error:`, data.error)
  return data.result
}

async function fetchIndexerTxStatus(hyliTxHash: string): Promise<string | undefined> {
  try {
    const res = await fetch(`${HYLI_INDEXER_URL}/v1/indexer/transaction/hash/${hyliTxHash}`)
    if (!res.ok) return undefined
    const data = await res.json()
    return data.transaction_status as string
  } catch {
    return undefined
  }
}

async function fetchHyliTxByMessageId(messageId: `0x${string}`): Promise<string | undefined> {
  try {
    const result = await rpcCall('hyli_getTxByMessageId', [messageId])
    if (!result) return undefined
    return (result as { hyliTxHash: string }).hyliTxHash
  } catch {
    return undefined
  }
}

// keccak256("DispatchId(bytes32)") — emitted by the Sepolia mailbox with the messageId
const DISPATCH_ID_TOPIC = keccak256(toBytes('DispatchId(bytes32)'))

function extractMessageId(
  logs: readonly { address: string; topics: readonly string[] }[],
): `0x${string}` | undefined {
  const log = logs.find(
    l =>
      l.address.toLowerCase() === SEPOLIA_MAILBOX.toLowerCase() &&
      l.topics[0]?.toLowerCase() === DISPATCH_ID_TOPIC.toLowerCase(),
  )
  return log?.topics[1] as `0x${string}` | undefined
}

export function useBridge() {
  const { address, chainId } = useAccount()
  // Log the RPC URL once so misconfiguration is immediately visible in the console
  useEffect(() => { console.log('[hyli rpc] url:', HYLI_RPC_URL) }, [])
  const { switchChainAsync } = useSwitchChain()
  const { writeContractAsync } = useWriteContract()
  const [status, setStatus] = useState<BridgeStatus>({ type: 'idle' })

  const relayInfo = useRef<{
    txHash: `0x${string}`
    messageId?: `0x${string}`
  } | null>(null)

  const { data: interchainFee = 0n } = useReadContract({
    address: SEPOLIA_WARP_CONTRACT,
    abi: QUOTE_GAS_PAYMENT_ABI,
    functionName: 'quoteGasPayment',
    args: [HYLI_DOMAIN],
    chainId: SEPOLIA_CHAIN_ID,
  })

  // Wait for Sepolia tx to be mined
  const confirmingHash = status.type === 'confirming' ? status.txHash : undefined
  const { data: sepoliaReceipt, error: sepoliaReceiptError } = useWaitForTransactionReceipt({
    hash: confirmingHash,
    chainId: SEPOLIA_CHAIN_ID,
  })

  // React to Sepolia receipt
  useEffect(() => {
    if (!confirmingHash) return
    if (sepoliaReceiptError) {
      setStatus({ type: 'error', message: sepoliaReceiptError.message })
      return
    }
    if (!sepoliaReceipt) return
    if (sepoliaReceipt.status === 'reverted') {
      setStatus({ type: 'sepolia_reverted', txHash: confirmingHash })
    } else {
      // Extract messageId from DispatchId log so we can poll hyli_getTxByMessageId
      if (relayInfo.current) {
        relayInfo.current.messageId = extractMessageId(sepoliaReceipt.logs)
        console.log('[hyli poll] messageId:', relayInfo.current.messageId)
      }
      setStatus({ type: 'relaying', txHash: confirmingHash })
    }
  }, [sepoliaReceipt, sepoliaReceiptError, confirmingHash])

  // Poll for relay sequencing then settlement
  useEffect(() => {
    if (status.type !== 'relaying') return
    const info = relayInfo.current
    if (!info) return

    let cancelled = false
    let hyliTxHash: string | undefined

    const timeoutId = setTimeout(() => {
      if (!cancelled) setStatus({ type: 'hyli_timeout', txHash: info.txHash, hyliTxHash })
    }, 120_000)

    ;(async () => {
      while (!cancelled) {
        await new Promise(r => setTimeout(r, 3_000))
        if (cancelled) break

        // Step 1: resolve the Hyli tx hash (available once the relayer sequences the tx)
        if (!hyliTxHash && info.messageId) {
          const found = await fetchHyliTxByMessageId(info.messageId)
          console.log('[hyli poll] messageId lookup:', found)
          if (found) {
            hyliTxHash = found
            if (!cancelled) setStatus({ type: 'relaying', txHash: info.txHash, hyliTxHash })
          }
        }

        // Step 2: poll the indexer for settlement
        if (hyliTxHash) {
          const txStatus = await fetchIndexerTxStatus(hyliTxHash)
          console.log('[hyli poll] indexer status:', txStatus)
          if (txStatus === 'Success') {
            clearTimeout(timeoutId)
            if (!cancelled) setStatus({ type: 'hyli_success', txHash: info.txHash, hyliTxHash })
            return
          }
        }
      }
    })()

    return () => {
      cancelled = true
      clearTimeout(timeoutId)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status.type])

  async function bridge(amountEth: string, recipient?: `0x${string}`) {
    if (!address) {
      setStatus({ type: 'error', message: 'Wallet not connected' })
      return
    }

    const recipientAddr = recipient ?? address

    try {
      if (chainId !== SEPOLIA_CHAIN_ID) {
        setStatus({ type: 'switching_chain' })
        await switchChainAsync({ chainId: SEPOLIA_CHAIN_ID })
      }

      setStatus({ type: 'pending' })

      const amountWei = parseEther(amountEth)
      const totalValue = amountWei + interchainFee

      const txHash = await writeContractAsync({
        address: SEPOLIA_WARP_CONTRACT,
        abi: TRANSFER_REMOTE_ABI,
        functionName: 'transferRemote',
        args: [HYLI_DOMAIN, encodeRecipient(recipientAddr), amountWei],
        value: totalValue,
        chainId: SEPOLIA_CHAIN_ID,
      })

      relayInfo.current = { txHash }
      setStatus({ type: 'confirming', txHash })
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Unknown error'
      setStatus({ type: 'error', message })
    }
  }

  function reset() {
    relayInfo.current = null
    setStatus({ type: 'idle' })
  }

  return {
    bridge,
    reset,
    status,
    interchainFee,
    isOnSepolia: chainId === SEPOLIA_CHAIN_ID,
  }
}
