export const CURRENCY = "EUR";

export const MAX_ITEMS_PER_CART = 50;

// How often createOrder retries a failed insert.
export const ORDER_RETRY_LIMIT = 3;

export const SESSION_TTL_MINUTES = 60 * 24;

export const LOGIN_ATTEMPTS_PER_HOUR = 10;

// Orders at or above this subtotal ship for free. null turns it off.
export const FREE_SHIPPING_THRESHOLD: number | null = null;
