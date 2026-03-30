import { NextResponse } from 'next/server'
import { DEFAULT_HYLI_INDEXER_URL, DEFAULT_HYLI_RPC_URL } from '@/lib/runtimeConfig'

export const dynamic = 'force-dynamic'

export async function GET() {
  return NextResponse.json({
    hyliRpcUrl: process.env.HYLI_RPC_URL ?? process.env.NEXT_PUBLIC_HYLI_RPC_URL ?? DEFAULT_HYLI_RPC_URL,
    hyliIndexerUrl: process.env.HYLI_INDEXER_URL ?? process.env.NEXT_PUBLIC_HYLI_INDEXER_URL ?? DEFAULT_HYLI_INDEXER_URL,
  })
}
