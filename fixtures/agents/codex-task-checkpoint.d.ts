// Generated from the Codex MCP tools/list inputSchema; Rust validation remains authoritative.
export type TaskCheckpointInput = {
  agent_kind: string;
  claims: Array<{
      conditions: Array<string>;
      context_kind: "decision" | "contract" | "issue" | "risk" | "validation" | "discovery" | "progress";
      evidence: Array<{
          evidence_type: "source_snapshot" | "experiment_record" | "artifact_snapshot";
          limitations: Array<string>;
          summary: string;
        }>;
      rationale: string;
      statement: string;
    }>;
  external_session_id: string;
  unknowns: Array<{
      blocking: boolean;
      statement: string;
    }>;
};
