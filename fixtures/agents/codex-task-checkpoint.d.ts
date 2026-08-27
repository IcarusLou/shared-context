// Generated from the Codex MCP tools/list inputSchema; Rust validation remains authoritative.
export type TaskCheckpointInput = {
  agent_kind: string;
  boundary: "continue" | "close";
  claims: Array<{
      applicability: {
        conditions: Array<string>;
        domains: Array<string>;
        platforms: Array<string>;
      };
      artifact_refs: Array<{
          locator: (
            | {
              locator_kind: "file";
              path: string;
            }
            | {
              locator_kind: "module";
              path: string;
            }
            | {
              locator_kind: "api";
              normalized_route: string;
              operation: string;
              path: string;
              protocol: string;
            }
            | {
              locator_kind: "schema";
              namespace: string;
              path: string;
              qualified_name: string;
              version: string;
            }
            | {
              enclosing_type: string | null;
              language: string;
              locator_kind: "symbol";
              module: string;
              path: string;
              signature: string;
              symbol_name: string;
            }
            | {
              locator_kind: "test";
              path: string;
              qualified_test_name: string;
            }
          );
          repository_id: string;
        }>;
      assumptions: Array<string>;
      context_kind_hint?: "decision" | "contract" | "issue" | "risk" | "validation" | "discovery" | "progress";
      engineering_references?: Array<{
          artifact_kind: "module" | "file" | "symbol" | "api" | "schema" | "test";
          limitations: Array<string>;
          locator: (
            | {
              locator_kind: "file";
              path: string;
            }
            | {
              locator_kind: "module";
              path: string;
            }
            | {
              locator_kind: "api";
              normalized_route: string;
              operation: string;
              path: string;
              protocol: string;
            }
            | {
              locator_kind: "schema";
              namespace: string;
              path: string;
              qualified_name: string;
              version: string;
            }
            | {
              enclosing_type: string | null;
              language: string;
              locator_kind: "symbol";
              module: string;
              path: string;
              signature: string;
              symbol_name: string;
            }
            | {
              locator_kind: "test";
              path: string;
              qualified_test_name: string;
            }
          );
          relation: "implements" | "defines" | "consumes" | "validates" | "constrains" | "depends_on";
          repository_id: string;
          supports: string;
        }>;
      evidence: Array<(
          | {
            capture_id: string;
            kind: "capture";
          }
          | {
            kind: "observation";
            observation_id: string;
          }
          | {
            kind: "task_signal";
            signal_id: string;
          }
          | {
            context_id: string;
            evidence_id: string;
            kind: "context_evidence";
            revision_id: string;
          }
          | {
            evidence: {
              content: Record<string, unknown>;
              interpretation: string;
              kind: "source_snapshot" | "experiment_record" | "artifact_snapshot";
              limitations: Array<string>;
              supports: string;
            };
            kind: "inline_validation";
          }
        )>;
      rationale: string;
      recheck_when: Array<string>;
      related_contexts: Array<{
          context_id: string;
          revision_id: string;
        }>;
      relations?: Array<{
          kind: "depends_on" | "constrains" | "implements" | "validated_by" | "contradicts" | "related_to";
          rationale: string;
          supports: Array<string>;
          target_context_id: string;
        }>;
      statement: string;
      topic_key_hint?: string;
    }>;
  expected_episode_version: number;
  expected_intent_revision_id: string;
  expected_task_id: string;
  external_session_id: string;
  unknowns: Array<{
      blocking: boolean;
      recheck_when: Array<string>;
      statement: string;
    }>;
};
