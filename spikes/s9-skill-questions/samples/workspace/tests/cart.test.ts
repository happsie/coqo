import { Cart } from "../src/cart/cart";

test("volume discount applies from ten items", () => {
  const cart = new Cart();
  cart.addItem({ sku: "a", unitPrice: 10, quantity: 10 });
  expect(cart.total()).toBe(95);
});

test("a coupon takes a percentage off", () => {
  const cart = new Cart();
  cart.addItem({ sku: "a", unitPrice: 10, quantity: 1 });
  cart.applyCoupon({ code: "TEN", percent: 10 });
  expect(cart.total()).toBe(9);
});
