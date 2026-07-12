declare module 'bun:test' {
  export function describe(name: string, callback: () => void): void
  export function test(name: string, callback: () => void | Promise<void>): void

  interface Matchers {
    toBe(expected: unknown): void
    toBeNull(): void
    toContain(expected: string): void
  }

  export function expect(actual: unknown): Matchers
}
