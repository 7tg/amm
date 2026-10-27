/** Named counting semaphores whose holders keep their slot only while their lease is renewed. */
export class Semaphores {
  private held = new Map<string, Map<string, number>>();
  private now: () => number;

  constructor(now: () => number = Date.now) {
    this.now = now;
  }

  tryAcquire(name: string, holder: string, capacity: number, leaseMs: number): boolean {
    const h = this.held.get(name) ?? new Map<string, number>();
    this.held.set(name, h);
    if (!h.has(holder) && h.size >= capacity) return false;
    h.set(holder, this.now() + leaseMs);
    return true;
  }

  renew(holder: string, leaseMs: number): void {
    for (const h of this.held.values()) if (h.has(holder)) h.set(holder, this.now() + leaseMs);
  }

  release(holder: string): void {
    for (const h of this.held.values()) h.delete(holder);
  }

  holders(name: string): string[] {
    return [...(this.held.get(name)?.keys() ?? [])];
  }

  /** Removes and returns every holder whose lease has run out. */
  expire(): string[] {
    const out = new Set<string>();
    for (const h of this.held.values()) {
      for (const [holder, until] of h) {
        if (until <= this.now()) {
          h.delete(holder);
          out.add(holder);
        }
      }
    }
    return [...out];
  }
}
