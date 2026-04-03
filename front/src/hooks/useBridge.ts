'use client'

import { useState, useEffect, useRef } from 'react'
import { parseEther, keccak256, toBytes } from 'viem'
import { useWriteContract, useReadContract, useAccount, useSwitchChain, useWaitForTransactionReceipt } from 'wagmi'
import {
  HYLI_DOMAIN,
  HYLI_CHAIN_ID,
  SEPOLIA_DOMAIN,
  SEPOLIA_CHAIN_ID,
  TRANSFER_REMOTE_ABI,
  QUOTE_GAS_PAYMENT_ABI,
  encodeRecipient,
} from '@/lib/hyperlane'
import { useRuntimeConfig } from '@/lib/runtimeConfig'

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

const DISPATCH_ID_TOPIC = keccak256(toBytes('DispatchId(bytes32)'))
const PROCESS_ID_TOPIC = keccak256(toBytes('ProcessId(bytes32)'))

function buildHyliRpcUnavailableMessage(hyliRpcUrl: string, error?: unknown) {
  const detail = error instanceof Error && error.message ? ` ${error.message}` : ''
  return `Hyli RPC is not accessible at ${hyliRpcUrl}. Check the frontend runtime config and Hyli ingress.${detail}`
}

function extractErrorMessage(error: unknown) {
  return error instanceof Error ? error.message : 'Unknown error'
}

async function hyliRpcCall(hyliRpcUrl: string, method: string, params: unknown[]): Promise<unknown> {
  let response: Response

  try {
    response = await fetch(hyliRpcUrl, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
    })
  } catch (error) {
    throw new Error(buildHyliRpcUnavailableMessage(hyliRpcUrl, error))
  }

  if (!response.ok) {
    throw new Error(buildHyliRpcUnavailableMessage(hyliRpcUrl, new Error(`HTTP ${response.status}`)))
  }

  const data = await response.json()
  if (data.error) console.warn(`[hyli rpc] ${method} error:`, data.error)
  return data.result
}

async function checkHyliRpcAvailability(hyliRpcUrl: string) {
  const chainId = await hyliRpcCall(hyliRpcUrl, 'eth_chainId', [])
  if (typeof chainId !== 'string' || chainId.length === 0) {
    throw new Error(buildHyliRpcUnavailableMessage(hyliRpcUrl))
  }
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

async function fetchIndexerTxStatus(hyliIndexerUrl: string, hyliTxHash: string): Promise<string | undefined> {
  try {
    const response = await fetch(`${hyliIndexerUrl}/v1/indexer/transaction/hash/${hyliTxHash}`)
    if (!response.ok) return undefined
    const data = await response.json()
    return data.transaction_status as string
  } catch {
    return undefined
  }
}

async function fetchHyliTxByMessageId(hyliRpcUrl: string, messageId: `0x${string}`): Promise<string | undefined> {
  try {
    const result = await hyliRpcCall(hyliRpcUrl, 'hyli_getTxByMessageId', [messageId])
    if (!result) return undefined
    return (result as { hyliTxHash: string }).hyliTxHash
  } catch {
    return undefined
  }
}

async function fetchHyliHashByEvmHash(hyliRpcUrl: string, evmHash: `0x${string}`): Promise<string | undefined> {
  try {
    const result = await hyliRpcCall(hyliRpcUrl, 'hyli_getHyliHash', [evmHash])
    if (!result) return undefined
    return (result as { hyliTxHash: string }).hyliTxHash
  } catch {
    return undefined
  }
}

async function pollSepoliaProcessId(
  messageId: `0x${string}`,
  sepoliaRpcUrl: string,
  sepoliaMailbox: `0x${string}`,
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
        jsonrpc: '2.0',
        id: 2,
        method: 'eth_getLogs',
        params: [{ address: sepoliaMailbox, topics: [PROCESS_ID_TOPIC, messageId], fromBlock, toBlock: 'latest' }],
      }),
    })
    const { result: logs = [] }: { result: { transactionHash: string }[] } = await logsRes.json()
    if (logs.length > 0) return logs[0].transactionHash
    fromBlock = latestHex
  }
  return undefined
}

export function useBridge(direction: Direction) {
  const {
    hyliRpcUrl,
    hyliIndexerUrl,
    sepoliaRpcUrl,
    sepoliaWarpContract,
    sepoliaMailbox,
    hyliWarpContract,
    hyliMailbox,
  } = useRuntimeConfig()
  const toHyli = direction === 'to_hyli'
  const sourceChainId = toHyli ? SEPOLIA_CHAIN_ID : HYLI_CHAIN_ID
  const warpContract = toHyli ? sepoliaWarpContract : hyliWarpContract
  const destDomain = toHyli ? HYLI_DOMAIN : SEPOLIA_DOMAIN
  const sourceMailbox = toHyli ? sepoliaMailbox : hyliMailbox

  const { address, chainId } = useAccount()
  const { switchChainAsync } = useSwitchChain()
  const { writeContractAsync } = useWriteContract()
  const [status, setStatus] = useState<BridgeStatus>({ type: 'idle' })
  const [rpcError, setRpcError] = useState<string | null>(null)

  const relayInfo = useRef<{ txHash: `0x${string}`; hyliSrcHash?: string; messageId?: `0x${string}` } | null>(null)

  useEffect(() => {
    console.log('[hyli rpc] url:', hyliRpcUrl)
  }, [hyliRpcUrl])

  useEffect(() => {
    let cancelled = false

    ;(async () => {
      try {
        await checkHyliRpcAvailability(hyliRpcUrl)
        if (!cancelled) setRpcError(null)
      } catch (error) {
        if (!cancelled) setRpcError(extractErrorMessage(error))
      }
    })()

    return () => {
      cancelled = true
    }
  }, [hyliRpcUrl])

  const { data: interchainFee = 0n } = useReadContract({
    address: warpContract,
    abi: QUOTE_GAS_PAYMENT_ABI,
    functionName: 'quoteGasPayment',
    args: [destDomain],
    chainId: sourceChainId,
    query: { enabled: true },
  })

  const confirmingHash = toHyli && status.type === 'confirming' ? status.txHash : undefined
  const { data: sepoliaReceipt, error: sepoliaReceiptError } = useWaitForTransactionReceipt({
    hash: confirmingHash,
    chainId: SEPOLIA_CHAIN_ID,
  })

  useEffect(() => {
    if (!confirmingHash) return
    if (sepoliaReceiptError) {
      setStatus({ type: 'error', message: sepoliaReceiptError.message })
      return
    }
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
      while (!cancelled) {
        await new Promise(r => setTimeout(r, 3_000))
        if (cancelled) break
        const hyliHash = info.hyliSrcHash
        if (!hyliHash) continue
        const txStatus = await fetchIndexerTxStatus(hyliIndexerUrl, hyliHash)
        console.log('[bridge] hyli indexer status:', txStatus)
        if (txStatus === 'Success') break
        if (txStatus && txStatus !== 'Sequenced') {
          setStatus({ type: 'source_reverted', txHash: info.txHash, hyliSrcHash: hyliHash })
          clearTimeout(timeoutId)
          return
        }
      }
      if (cancelled) return

      const receipt = await hyliRpcCall(hyliRpcUrl, 'eth_getTransactionReceipt', [info.txHash]) as
        { logs: { address: string; topics: string[] }[] } | null
      if (cancelled) return
      if (!receipt) {
        setStatus({ type: 'source_reverted', txHash: info.txHash, hyliSrcHash: info.hyliSrcHash })
        clearTimeout(timeoutId)
        return
      }

      const messageId = extractDispatchId(receipt.logs, sourceMailbox)
      console.log('[bridge] hyli messageId:', messageId)
      if (!messageId) {
        setStatus({ type: 'error', message: 'No DispatchId event in Hyli receipt' })
        clearTimeout(timeoutId)
        return
      }

      info.messageId = messageId
      clearTimeout(timeoutId)
      setStatus({ type: 'relaying', txHash: info.txHash, hyliSrcHash: info.hyliSrcHash })
    })().catch(error => {
      clearTimeout(timeoutId)
      if (!cancelled) setStatus({ type: 'error', message: extractErrorMessage(error) })
    })

    return () => {
      cancelled = true
      clearTimeout(timeoutId)
    }
  }, [hyliIndexerUrl, hyliRpcUrl, sourceMailbox, status.type, toHyli])

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
        while (!cancelled) {
          await new Promise(r => setTimeout(r, 3_000))
          if (cancelled) break

          if (!destTxHash && info.messageId) {
            const found = await fetchHyliTxByMessageId(hyliRpcUrl, info.messageId)
            console.log('[bridge] hyli tx lookup:', found)
            if (found) {
              destTxHash = found
              if (!cancelled) setStatus({ type: 'relaying', txHash: info.txHash, destTxHash })
            }
          }

          if (destTxHash) {
            const txStatus = await fetchIndexerTxStatus(hyliIndexerUrl, destTxHash)
            console.log('[bridge] hyli indexer status:', txStatus)
            if (txStatus === 'Success') {
              clearTimeout(timeoutId)
              if (!cancelled) setStatus({ type: 'success', txHash: info.txHash, destTxHash })
              return
            }
          }
        }
      } else {
        const messageId = info.messageId
        if (!messageId) {
          setStatus({ type: 'error', message: 'No DispatchId event in Hyli receipt' })
          clearTimeout(timeoutId)
          return
        }

        const sepoliaTx = await pollSepoliaProcessId(messageId, sepoliaRpcUrl, sepoliaMailbox, 110_000)
        if (cancelled) return
        clearTimeout(timeoutId)
        if (sepoliaTx) setStatus({ type: 'success', txHash: info.txHash, hyliSrcHash, destTxHash: sepoliaTx })
        else setStatus({ type: 'timeout', txHash: info.txHash, hyliSrcHash })
      }
    })().catch(error => {
      clearTimeout(timeoutId)
      if (!cancelled) setStatus({ type: 'error', message: extractErrorMessage(error) })
    })

    return () => {
      cancelled = true
      clearTimeout(timeoutId)
    }
  }, [hyliIndexerUrl, hyliRpcUrl, sepoliaMailbox, sepoliaRpcUrl, status.type, toHyli])

  async function bridge(amountEth: string, recipient?: `0x${string}`) {
    if (!address) {
      setStatus({ type: 'error', message: 'Wallet not connected' })
      return
    }
    if (rpcError) {
      setStatus({ type: 'error', message: rpcError })
      return
    }

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
        fetchHyliHashByEvmHash(hyliRpcUrl, txHash).then(hyliSrcHash => {
          if (hyliSrcHash && relayInfo.current) {
            relayInfo.current.hyliSrcHash = hyliSrcHash
            setStatus(prev => 'txHash' in prev ? { ...prev, hyliSrcHash } : prev)
          }
        })
      }
    } catch (error) {
      setStatus({ type: 'error', message: extractErrorMessage(error) })
    }
  }

  function reset() {
    relayInfo.current = null
    setStatus({ type: 'idle' })
  }

  return { bridge, reset, status, interchainFee, rpcError }
}
