import { clsx, type ClassValue } from 'clsx'
import { twMerge } from 'tailwind-merge'

/** Merge class lists so a caller's spacing wins over a variant's default. */
export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs))
}
