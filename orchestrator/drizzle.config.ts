import { defineConfig } from "drizzle-kit";

export default defineConfig({
  dialect: "postgresql",
  schema: "src/db/schema.ts",
  out: "drizzle/",
  dbCredentials: {
    url: process.env["ORCHESTRATOR_DATABASE_URL"] ?? "",
  },
});
