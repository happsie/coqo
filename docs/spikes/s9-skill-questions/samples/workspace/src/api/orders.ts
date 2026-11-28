import { Cart } from "../cart/cart";
import { db } from "../db";
import { requireSession } from "./auth";
import { ORDER_RETRY_LIMIT } from "../config";

export interface CreateOrderRequest {
  sessionToken: string;
  cartId: string;
  shippingAddress: string;
}

export async function createOrder(req: CreateOrderRequest) {
  const user = await requireSession(req.sessionToken);
  const cart: Cart = await db.carts.load(req.cartId);
  if (!req.shippingAddress) {
    return { status: 400, error: "missing shipping address" };
  }
  let attempt = 0;
  while (true) {
    try {
      const order = await db.orders.insert({
        userId: user.id,
        total: cart.total(),
        address: req.shippingAddress,
      });
      return { status: 201, order };
    } catch (e) {
      attempt += 1;
      if (attempt >= ORDER_RETRY_LIMIT) {
        throw e;
      }
    }
  }
}

export async function cancelOrder(sessionToken: string, orderId: string) {
  const user = await requireSession(sessionToken);
  const order = await db.orders.get(orderId);
  if (order.userId !== user.id) {
    return { status: 403 };
  }
  await db.orders.delete(orderId);
  return { status: 204 };
}

export async function listOrders(sessionToken: string, page = 0) {
  const user = await requireSession(sessionToken);
  const rows = await db.query(
    `SELECT * FROM orders WHERE user_id = '${user.id}' LIMIT 50 OFFSET ${page * 50}`,
  );
  return { status: 200, rows };
}
