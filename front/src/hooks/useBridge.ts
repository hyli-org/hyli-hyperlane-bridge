'use client'

import { useState, useEffect, useRef } from 'react'
import { parseEther, keccak256, toBytes } from 'viem'
import { useWriteContract, useReadContract, useAccount, useSwitchChain, useWaitForTransactionReceipt } from 'wagmi'
import {
  SEPOLIA_WARP_CONTRACT,
  SEPOLIA_MAILBOX,
  HYLI_WARP_CONTRACT,
  HYLI_MAILBOX,
  HYLI_DOMAIN,
  HYLI_CHAIN_ID,
  SEPOLIA_DOMAIN,
  SEPOLIA_CHAIN_ID,
  HYLI_RPC_URL,
  HYLI_INDEXER_URL,
  TRANSFER_REMOTE_ABI,
  QUOTE_GAS_PAYMENT_ABI,
  encodeRecipient,
} from '@/lib/hyperlane'

export type Direction = 'to_hyli' | 'to_sepolia'

export type BridgeStatus =
  | { type: 'idle' }
  | { type: 'switching_chain' }
  | { type: 'pending' }
  | { type: 'confirming'; txHash: `0x${string}`; hyliSrcHash?: string }
  | { type: 'source_reverted'; txHash: `0x${string}`; hyliSrcHash?: string }
  | { type: 'relaying'; txHash: `0x${string}`; hyliSrcHash?: string; destTxHash?: string }
  | { type: 'success'; txHash: `0x${string}`; hyliSrcHash?: string; destTxHash?: string }
  | { type: 'timeout'; txHash: `0x${string}`; hyliSrcHash?: string; destTxHash?: string }
  | { type: 'error'; message: string }

// ── Helpers ───────────────────────────────────────────────────────────────────

const DISPATCH_ID_TOPIC = keccak256(toBytes('DispatchId(bytes32)'))
const PROCESS_ID_TOPIC  = keccak256(toBytes('ProcessId(bytes32)'))

async function hyliRpcCall(method: string, params: unknown[]): Promise<unknown> {
  const res = await fetch(HYLI_RPC_URL, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
  })
  const data = await res.json()
  if (data.error) console.warn(`[hyli rpc] ${method} error:`, data.error)
  return data.result
}

function extractDispatchId(
  logs: readonly { address: string; topics: readonly string[] }[],
  mailbox: string,
): `0x${string}` | undefined {
  const log = logs.find(
    l =>
      l.address.toLowerCase() === mailbox.toLowerCase() &&
      l.topics[0]?.toLowerCase() === DISPATCH_ID_TOPIC.toLowerCase(),
  )
  return log?.topics[1] as `0x${string}` | undefined
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
    const result = await hyliRpcCall('hyli_getTxByMessageId', [messageId])
    if (!result) return undefined
    return (result as { hyliTxHash: string }).hyliTxHash
  } catch {
    return undefined
  }
}

async function fetchHyliHashByEvmHash(evmHash: `0x${string}`): Promise<string | undefined> {
  try {
    const result = await hyliRpcCall('hyli_getHyliHash', [evmHash])
    if (!result) return undefined
    return (result as { hyliTxHash: string }).hyliTxHash
  } catch {
    return undefined
  }
}

async function pollSepoliaProcessId(
  messageId: `0x${string}`,
  sepoliaRpcUrl: string,
  timeoutMs = 120_000,
): Promise<string | undefined> {
  const deadline = Date.now() + timeoutMs
  let fromBlock: string | null = null

  while (Date.now() < deadline) {
    await new Promise(r => setTimeout(r, 5_000))
    const blockNumRes = await fetch(sepoliaRpcUrl, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'eth_blockNumber', params: [] }),
    })
    const { result: latestHex = '0x0' } = await blockNumRes.json()
    const latest = parseInt(latestHex, 16)
    if (!fromBlock) fromBlock = '0x' + Math.max(0, latest - 20).toString(16)

    const logsRes = await fetch(sepoliaRpcUrl, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        jsonrpc: '2.0', id: 2, method: 'eth_getLogs',
        params: [{ address: SEPOLIA_MAILBOX, topics: [PROCESS_ID_TOPIC, messageId], fromBlock, toBlock: 'latest' }],
      }),
    })
    const { result: logs = [] }: { result: { transactionHash: string }[] } = await logsRes.json()
    if (logs.length > 0) return logs[0].transactionHash
    fromBlock = latestHex
  }
  return undefined
}

// ── Hook ─────────────────────────────────────────────────────────────────────

export function useBridge(direction: Direction) {
  const toHyli = direction === 'to_hyli'
  const sourceChainId  = toHyli ? SEPOLIA_CHAIN_ID : HYLI_CHAIN_ID
  const warpContract   = toHyli ? SEPOLIA_WARP_CONTRACT : HYLI_WARP_CONTRACT
  const destDomain     = toHyli ? HYLI_DOMAIN : SEPOLIA_DOMAIN
  const sourceMailbox  = toHyli ? SEPOLIA_MAILBOX : HYLI_MAILBOX

  const { address, chainId } = useAccount()
  useEffect(() => { console.log('[hyli rpc] url:', HYLI_RPC_URL) }, [])
  const { switchChainAsync } = useSwitchChain()
  const { writeContractAsync } = useWriteContract()
  const [status, setStatus] = useState<BridgeStatus>({ type: 'idle' })

  const relayInfo = useRef<{ txHash: `0x${string}`; hyliSrcHash?: string; messageId?: `0x${string}` } | null>(null)

  // quoteGasPayment is only meaningful on Sepolia; Hyli has no interchain gas fee
  const { data: interchainFee = 0n } = useReadContract({
    address: warpContract,
    abi: QUOTE_GAS_PAYMENT_ABI,
    functionName: 'quoteGasPayment',
    args: [destDomain],
    chainId: sourceChainId,
    query: { enabled: toHyli },
  })

  // Sepolia -> Hyli: use wagmi's built-in receipt watcher for the source tx
  const confirmingHash = toHyli && status.type === 'confirming' ? status.txHash : undefined
  const { data: sepoliaReceipt, error: sepoliaReceiptError } = useWaitForTransactionReceipt({
    hash: confirmingHash,
    chainId: SEPOLIA_CHAIN_ID,
  })

  useEffect(() => {
    if (!confirmingHash) return
    if (sepoliaReceiptError) { setStatus({ type: 'error', message: sepoliaReceiptError.message }); return }
    if (!sepoliaReceipt) return
    if (sepoliaReceipt.status === 'reverted') {
      setStatus({ type: 'source_reverted', txHash: confirmingHash })
    } else {
      if (relayInfo.current) {
        relayInfo.current.messageId = extractDispatchId(sepoliaReceipt.logs, sourceMailbox)
        console.log('[bridge] messageId:', relayInfo.current.messageId)
      }
      setStatus({ type: 'relaying', txHash: confirmingHash })
    }
  }, [sepoliaReceipt, sepoliaReceiptError, confirmingHash, sourceMailbox])

  // Hyli -> Sepolia: poll Hyli indexer until the source tx settles, then get receipt for logs
  useEffect(() => {
    if (toHyli) return
    if (status.type !== 'confirming') return
    const info = relayInfo.current
    if (!info) return

    let cancelled = false

    const timeoutId = setTimeout(() => {
      if (!cancelled) setStatus({ type: 'timeout', txHash: info.txHash, hyliSrcHash: info.hyliSrcHash })
    }, 120_000)

    ;(async () => {
      // Poll indexer until the Hyli tx is settled
      while (!cancelled) {
        await new Promise(r => setTimeout(r, 3_000))
        if (cancelled) break
        const hyliHash = info.hyliSrcHash
        if (!hyliHash) continue
        const txStatus = await fetchIndexerTxStatus(hyliHash)
        console.log('[bridge] hyli indexer status:', txStatus)
        if (txStatus === 'Success') break
        if (txStatus && txStatus !== 'Sequenced') {
          // Failed / unknown terminal state
          setStatus({ type: 'source_reverted', txHash: info.txHash, hyliSrcHash: hyliHash })
          clearTimeout(timeoutId)
          return
        }
      }
      if (cancelled) return

      // Fetch receipt once to extract the DispatchId messageId from logs
      const receipt = await hyliRpcCall('eth_getTransactionReceipt', [info.txHash]) as
        { logs: { address: string; topics: string[] }[] } | null
      if (cancelled) return
      if (!receipt) { setStatus({ type: 'source_reverted', txHash: info.txHash, hyliSrcHash: info.hyliSrcHash }); clearTimeout(timeoutId); return }

      const messageId = extractDispatchId(receipt.logs, sourceMailbox)
      console.log('[bridge] hyli messageId:', messageId)
      if (!messageId) { setStatus({ type: 'error', message: 'No DispatchId event in Hyli receipt' }); clearTimeout(timeoutId); return }

      info.messageId = messageId
      clearTimeout(timeoutId)
      setStatus({ type: 'relaying', txHash: info.txHash, hyliSrcHash: info.hyliSrcHash })
    })()

    return () => { cancelled = true; clearTimeout(timeoutId) }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status.type])

  // Relay polling — direction-specific
  useEffect(() => {
    if (status.type !== 'relaying') return
    const info = relayInfo.current
    if (!info) return

    let cancelled = false
    let destTxHash: string | undefined

    const hyliSrcHash = info.hyliSrcHash

    const timeoutId = setTimeout(() => {
      if (!cancelled) setStatus({ type: 'timeout', txHash: info.txHash, hyliSrcHash, destTxHash })
    }, 120_000)

    ;(async () => {
      if (toHyli) {
        // Sepolia -> Hyli: poll hyli_getTxByMessageId then indexer
        while (!cancelled) {
          await new Promise(r => setTimeout(r, 3_000))
          if (cancelled) break

          if (!destTxHash && info.messageId) {
            const found = await fetchHyliTxByMessageId(info.messageId)
            console.log('[bridge] hyli tx lookup:', found)
            if (found) {
              destTxHash = found
              if (!cancelled) setStatus({ type: 'relaying', txHash: info.txHash, destTxHash })
            }
          }

          if (destTxHash) {
            const txStatus = await fetchIndexerTxStatus(destTxHash)
            console.log('[bridge] hyli indexer status:', txStatus)
            if (txStatus === 'Success') {
              clearTimeout(timeoutId)
              if (!cancelled) setStatus({ type: 'success', txHash: info.txHash, destTxHash })
              return
            }
          }
        }
      } else {
        // Hyli -> Sepolia: messageId already set by the confirming effect; poll Sepolia ProcessId
        const messageId = info.messageId
        if (!messageId) { setStatus({ type: 'error', message: 'No DispatchId event in Hyli receipt' }); clearTimeout(timeoutId); return }

        const sepoliaRpcUrl = process.env.NEXT_PUBLIC_SEPOLIA_RPC_URL!
        const sepoliaTx = await pollSepoliaProcessId(messageId, sepoliaRpcUrl, 110_000)
        if (cancelled) return
        clearTimeout(timeoutId)
        if (sepoliaTx) setStatus({ type: 'success', txHash: info.txHash, hyliSrcHash, destTxHash: sepoliaTx })
        else setStatus({ type: 'timeout', txHash: info.txHash, hyliSrcHash })
      }
    })()

    return () => { cancelled = true; clearTimeout(timeoutId) }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status.type])

  async function bridge(amountEth: string, recipient?: `0x${string}`) {
    if (!address) { setStatus({ type: 'error', message: 'Wallet not connected' }); return }
    const recipientAddr = recipient ?? address
    try {
      if (chainId !== sourceChainId) {
        setStatus({ type: 'switching_chain' })
        await switchChainAsync({ chainId: sourceChainId })
      }
      setStatus({ type: 'pending' })
      const amountWei = parseEther(amountEth)

      const txHash = await writeContractAsync({
        address: warpContract,
        abi: TRANSFER_REMOTE_ABI,
        functionName: 'transferRemote',
        args: [destDomain, encodeRecipient(recipientAddr), amountWei],
        value: toHyli ? amountWei + interchainFee : interchainFee,
        chainId: sourceChainId,
      })
      relayInfo.current = { txHash }
      setStatus({ type: 'confirming', txHash })

      if (!toHyli) {
        // Hyli→Sepolia: fetch the Hyli blob tx hash for the explorer link and indexer polling.
        fetchHyliHashByEvmHash(txHash).then(hyliSrcHash => {
          if (hyliSrcHash && relayInfo.current) {
            relayInfo.current.hyliSrcHash = hyliSrcHash
            setStatus(prev => 'txHash' in prev ? { ...prev, hyliSrcHash } : prev)
          }
        })
      }
    } catch (err) {
      setStatus({ type: 'error', message: err instanceof Error ? err.message : 'Unknown error' })
    }
  }

  function reset() {
    relayInfo.current = null
    setStatus({ type: 'idle' })
  }

  return { bridge, reset, status, interchainFee }
}
