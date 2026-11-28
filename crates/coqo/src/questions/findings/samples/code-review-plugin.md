## Code review

Found 2 issues:

1. Session age compares minutes against a `createdAt` stored in seconds (bug: always expired or never)

https://github.com/acme/shop/blob/3f2c9a1d0e6b4c8f7a5d2e1b0c9f8e7d6a5b4c3d/src/api/auth.ts#L15-L18

2. Raw SQL with an interpolated user id (CLAUDE.md says "Always use parameterized queries")

https://github.com/acme/shop/blob/3f2c9a1d0e6b4c8f7a5d2e1b0c9f8e7d6a5b4c3d/src/api/orders.ts#L47-L51

🤖 Generated with [Claude Code](https://claude.ai/code)

<sub>- If this code review was useful, please react with 👍. Otherwise, react with 👎.</sub>
