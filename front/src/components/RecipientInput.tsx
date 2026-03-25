'use client'

interface RecipientInputProps {
  value: string
  onChange: (value: string) => void
  disabled?: boolean
  destChainName?: string
}

export function RecipientInput({ value, onChange, disabled, destChainName = 'Hyli' }: RecipientInputProps) {
  return (
    <div className="space-y-1.5">
      <label className="text-sm font-medium text-gray-300">
        Recipient on {destChainName}{' '}
        <span className="text-gray-500 font-normal">(optional — defaults to your address)</span>
      </label>
      <input
        type="text"
        placeholder="0x…"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        disabled={disabled}
        className="w-full bg-gray-800 border border-gray-600 rounded-lg px-4 py-3 text-white placeholder-gray-500 font-mono text-sm focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-transparent disabled:opacity-50"
      />
    </div>
  )
}
