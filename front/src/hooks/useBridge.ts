'use client'

import { useState } from 'react'
import { parseEther } from 'viem'
import { useWriteContract, useReadContract, useAccount, useSwitchChain } from 'wagmi'
import {
  SEPOLIA_WARP_CONTRACT,
  HYLI_DOMAIN,
  SEPOLIA_CHAIN_ID,
  TRANSFER_REMOTE_ABI,
  QUOTE_GAS_PAYMENT_ABI,
  encodeRecipient,
} from '@/lib/hyperlane'

export type BridgeStatus =
  | { type: 'idle' }
  | { type: 'switching_chain' }
  | { type: 'pending' }
  | { type: 'success'; txHash: `0x${string}` }
  | { type: 'error'; message: string }

export function useBridge() {
  const { address, chainId } = useAccount()
  const { switchChainAsync } = useSwitchChain()
  const { writeContractAsync } = useWriteContract()
  const [status, setStatus] = useState<BridgeStatus>({ type: 'idle' })

  // Quote interchain gas fee (usually 0 with TrustedRelayer ISM, but called for correctness)
  const { data: interchainFee = 0n } = useReadContract({
    address: SEPOLIA_WARP_CONTRACT,
    abi: QUOTE_GAS_PAYMENT_ABI,
    functionName: 'quoteGasPayment',
    args: [HYLI_DOMAIN],
    chainId: SEPOLIA_CHAIN_ID,
  })

  async function bridge(amountEth: string, recipient?: `0x${string}`) {
    if (!address) {
      setStatus({ type: 'error', message: 'Wallet not connected' })
      return
    }

    const recipientAddr = recipient ?? address

    try {
      // Switch to Sepolia if needed
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

      setStatus({ type: 'success', txHash })
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Unknown error'
      setStatus({ type: 'error', message })
    }
  }

  function reset() {
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
