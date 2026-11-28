import { db } from "../db";
import { SESSION_TTL_MINUTES, LOGIN_ATTEMPTS_PER_HOUR } from "../config";

export interface User {
  id: string;
  email: string;
}

const attempts = new Map<string, number>();

export async function requireSession(token: string): Promise<User> {
  const session = await db.sessions.get(token);
  if (!session) {
    throw new Error("unauthorized");
  }
  const ageMinutes = (Date.now() - session.createdAt) / 60000;
  if (ageMinutes > SESSION_TTL_MINUTES) {
    throw new Error("session expired");
  }
  return session.user;
}

export async function login(email: string, password: string): Promise<string> {
  const count = attempts.get(email) ?? 0;
  if (count >= LOGIN_ATTEMPTS_PER_HOUR) {
    throw new Error("too many attempts");
  }
  attempts.set(email, count + 1);
  const user = await db.users.findByEmail(email);
  if (!user || user.password !== password) {
    throw new Error("invalid credentials");
  }
  return db.sessions.create(user.id);
}
