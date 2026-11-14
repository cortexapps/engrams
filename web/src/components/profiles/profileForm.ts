import * as z from "zod";

export const integrationGrantSchema = z.object({
  connectionId: z.string(),
  operation: z.string(),
  resourceConstraints: z.array(z.string()),
  // ADR 0115: "" | "org" = the shared org credential; "user" = each
  // member's personal credential (human launches then require it).
  credentialScope: z.string().optional(),
  mcp: z
    .object({
      upstreamName: z.string(),
      automationWrite: z.boolean(),
      unrestricted: z.boolean(),
      argumentConstraintDocumentsJson: z.array(z.string()),
    })
    .optional(),
});

export type McpProfileGrant = z.infer<typeof integrationGrantSchema>;
