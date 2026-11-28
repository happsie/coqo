Reviewing the unstaged changes from `git diff` (3 files).

### Critical

- **Plain-text password check** (confidence 95): `src/api/auth.ts`, line 30. `user.password !== password` compares the stored value directly. Fix: compare a bcrypt hash.

### Important

- **Login attempts never reset** (confidence 85): in auth.ts (line 28) the counter only grows, so after 10 attempts a user is locked out for good, not for an hour. Fix: store a timestamp per attempt.
- **Retry loop without a delay** (confidence 80): lines 19-33 of `src/api/orders.ts` retry at once. Fix: back off.

The diff hunk `@@ -46,7 +46,7 @@` in orders.ts is fine. Version bumped from 1.4.2 to 1.5.0.

```diff
-    `SELECT * FROM orders WHERE user_id = '${user.id}'`,
+    "SELECT * FROM orders WHERE user_id = $1", [user.id],
```

Everything else meets the project's conventions (CLAUDE.md:12 asks for named exports, which the diff uses).
