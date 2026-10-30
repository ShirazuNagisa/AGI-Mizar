import { clsx, type ClassValue } from 'clsx'
import { twMerge } from 'tailwind-merge'

/** Merge class lists so a caller's spacing wins over a variant's default. */
export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs))
}

/** A failure's own message, or a stand-in when it is not one this window made. */
export function failureMessage(failure: unknown, fallback: string): string {
  return failure instanceof Error ? failure.message : fallback
}
