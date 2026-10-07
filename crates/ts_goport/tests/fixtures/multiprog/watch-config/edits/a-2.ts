// A point,
// in two lines.
export interface Point {
  x: number;
  y: number;
}

/** Makes a point. */
export function point(x: number, y: number): Point {
  return { x, y }; // both
}
