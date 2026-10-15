export enum Kind { Circle = "circle", Square = "square" }

export interface Shape {
  kind: Kind;
  area(): number;
}

export abstract class Base implements Shape {
  abstract kind: Kind;
  constructor(protected readonly size: number) {}
  abstract area(): number;
  describe(): string {
    return `${this.kind}(${this.size}) area=${this.area().toFixed(2)}`;
  }
}

export class Circle extends Base {
  kind = Kind.Circle;
  area(): number { return Math.PI * this.size ** 2; }
}

export class Square extends Base {
  kind = Kind.Square;
  area(): number { return this.size * this.size; }
}

export namespace Geometry {
  export const unit = 1;
  export function total(shapes: readonly Shape[]): number {
    return shapes.reduce((sum, s) => sum + s.area(), 0);
  }
}

export function explode(reason: string): never {
  throw new RangeError(reason);
}
