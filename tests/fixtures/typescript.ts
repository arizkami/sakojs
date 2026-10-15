// TypeScript entry point: types stripped, TS-only syntax lowered, ".js"
// specifiers resolved to ".ts" sources, stack traces in TS positions.
import { Circle, Square, Geometry, Kind, explode, type Shape } from "./lib/shapes.js";
import data from "./lib/data.json" with { type: "json" };

type Tuple<T> = readonly [T, T];
const pair: Tuple<number> = [3, 4] as const;
const shapes: Shape[] = [new Circle(1), new Square(2)];

function sum<T extends number>(...xs: T[]): number {
  return xs.reduce((a, b) => a + b, 0);
}

console.log(shapes.map((s) => (s as Circle | Square).describe()));
console.log(Geometry.total(shapes).toFixed(3), Geometry.unit, Kind.Square, sum(...pair));
console.log(data.items.length satisfies number, <string>"cast", typeof (pair as unknown));

const lazy = await import("./lib/shapes.ts");
console.log("dynamic import:", Object.keys(lazy).sort().join(","));

try {
  explode("too big");
} catch (e) {
  const frame = String((e as Error).stack).split("\n")[1];
  console.log((e as Error).name, frame.slice(frame.indexOf("shapes.ts")));
}
