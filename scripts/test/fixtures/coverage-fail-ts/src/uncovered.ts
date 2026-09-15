export function neverCalled(value: number): number {
  let total = value;
  total += 1;
  total *= 2;
  total -= 3;
  total /= 2;
  return total;
}

export function alsoNeverCalled(): string {
  const parts = ["coverage", "gate", "fixture"];
  return parts.join("-");
}
