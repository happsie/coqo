import { volumeDiscount, couponDiscount, Coupon } from "./discounts";
import { MAX_ITEMS_PER_CART, CURRENCY } from "../config";

export interface LineItem {
  sku: string;
  unitPrice: number;
  quantity: number;
}

export class Cart {
  private items: LineItem[] = [];
  private coupon: Coupon | null = null;

  addItem(item: LineItem): void {
    if (item.quantity <= 0) {
      throw new Error("quantity must be positive");
    }
    const existing = this.items.find((i) => i.sku === item.sku);
    if (existing) {
      existing.quantity += item.quantity;
      return;
    }
    if (this.items.length >= MAX_ITEMS_PER_CART) {
      throw new Error("cart is full");
    }
    this.items.push({ ...item });
  }

  removeItem(sku: string): void {
    this.items = this.items.filter((i) => i.sku !== sku);
  }

  applyCoupon(coupon: Coupon): void {
    // A second coupon replaces the first one.
    this.coupon = coupon;
  }

  subtotal(): number {
    return this.items.reduce(
      (sum, i) => sum + volumeDiscount(i.unitPrice, i.quantity) * i.quantity,
      0,
    );
  }

  total(): number {
    const subtotal = this.subtotal();
    const off = this.coupon ? couponDiscount(subtotal, this.coupon) : 0;
    return Math.round((subtotal - off) * 100) / 100;
  }

  summary(): string {
    return `${this.items.length} items, ${this.total().toFixed(2)} ${CURRENCY}`;
  }
}
