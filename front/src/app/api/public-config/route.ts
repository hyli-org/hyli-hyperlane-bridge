import { NextResponse } from 'next/server'
import {
  DEFAULT_ETHERSCAN_BASE_URL,
  DEFAULT_HYLI_EXPLORER_BASE_URL,
  DEFAULT_HYLI_INDEXER_URL,
  DEFAULT_HYLI_MAILBOX,
  DEFAULT_HYLI_RPC_URL,
  DEFAULT_HYLI_WARP_CONTRACT,
  DEFAULT_SEPOLIA_MAILBOX,
  DEFAULT_SEPOLIA_RPC_URL,
  DEFAULT_SEPOLIA_WARP_CONTRACT,
} from '@/lib/runtimeConfig'

export const dynamic = 'force-dynamic'

export async function GET() {
  const hyliWarpContract = process.env.HYLI_WARP_CONTRACT ?? DEFAULT_HYLI_WARP_CONTRACT

  return NextResponse.json({
    hyliRpcUrl: process.env.HYLI_RPC_URL ?? DEFAULT_HYLI_RPC_URL,
    hyliIndexerUrl: process.env.HYLI_INDEXER_URL ?? DEFAULT_HYLI_INDEXER_URL,
    sepoliaRpcUrl: process.env.SEPOLIA_RPC_URL ?? DEFAULT_SEPOLIA_RPC_URL,
    sepoliaWarpContract: process.env.SEPOLIA_WARP_CONTRACT ?? DEFAULT_SEPOLIA_WARP_CONTRACT,
    sepoliaMailbox: process.env.SEPOLIA_MAILBOX ?? DEFAULT_SEPOLIA_MAILBOX,
    hyliWarpContract,
    hyliMailbox: process.env.HYLI_MAILBOX ?? DEFAULT_HYLI_MAILBOX,
    etherscanBaseUrl: process.env.ETHERSCAN_BASE_URL ?? DEFAULT_ETHERSCAN_BASE_URL,
    hyliExplorerBaseUrl: process.env.HYLI_EXPLORER_BASE_URL ?? DEFAULT_HYLI_EXPLORER_BASE_URL,
  })
}
