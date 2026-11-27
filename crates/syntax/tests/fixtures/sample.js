import { join } from "path";

const LIMIT = 3;

export class Counter {
  count = 0;

  constructor(start) {
    this.count = start;
  }

  increment() {
    this.count += 1;
    return this.count;
  }

  static zero() {
    return new Counter(0);
  }
}

function greet(name) {
  return `Hello, ${name}`;
}

const shout = function (text) {
  return text.toUpperCase();
};

export const App = () => (
  <main>
    <p>{greet("world")}</p>
  </main>
);

module.exports = { greet, shout, LIMIT, join };
