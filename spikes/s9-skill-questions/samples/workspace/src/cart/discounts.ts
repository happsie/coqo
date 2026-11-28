export interface Coupon {
  code: string;
  percent?: number;
  amount?: number;
  minSubtotal?: number;
}

/** The unit price after the volume discount. */
export function volumeDiscount(unitPrice: number, quantity: number): number {
  if (quantity >= 100) {
    return unitPrice * 0.9;
  }
  if (quantity >= 10) {
    return unitPrice * 0.95;
  }
  return unitPrice;
}

/** How much a coupon takes off a subtotal. */
export function couponDiscount(subtotal: number, coupon: Coupon): number {
  if (coupon.minSubtotal !== undefined && subtotal < coupon.minSubtotal) {
    return 0;
  }
  if (coupon.percent !== undefined) {
    return subtotal * (coupon.percent / 100);
  }
  if (coupon.amount !== undefined) {
    return coupon.amount;
  }
  return 0;
}
