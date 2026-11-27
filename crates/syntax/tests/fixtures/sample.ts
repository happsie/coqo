import { readFile } from "fs";
import * as path from "path";

export interface Shape {
  area(): number;
  name: string;
}

export type Id = string | number;

enum Direction {
  Up,
  Down,
}

namespace Geometry {
  export const ORIGIN = { x: 0, y: 0 };
}

export abstract class Circle implements Shape {
  name = "circle";
  private radius: number;

  constructor(radius: number) {
    this.radius = radius;
  }

  area(): number {
    return Math.PI * this.radius ** 2;
  }
}

export function describe(shape: Shape): string {
  return `${shape.name}: ${shape.area()}`;
}

const double = (n: number): number => n * 2;

/**
 * Loads a file.
 */
async function load(file: string) {
  return readFile(path.join(".", file), () => {});
}
