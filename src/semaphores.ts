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
    const now = this.now();
    const live = [...h].filter(([k, until]) => k !== holder && until > now).length;
    if (live >= capacity) return false;
    h.set(holder, now + leaseMs);
    return true;
  }

  renew(holder: string, leaseMs: number): void {
    const now = this.now();
    for (const h of this.held.values()) if ((h.get(holder) ?? 0) > now) h.set(holder, now + leaseMs);
  }

  release(holder: string): void {
    for (const h of this.held.values()) h.delete(holder);
  }

  holders(name: string): string[] {
    const now = this.now();
    return [...(this.held.get(name) ?? [])].filter(([, until]) => until > now).map(([k]) => k);
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
