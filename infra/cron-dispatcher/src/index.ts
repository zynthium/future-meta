export interface Env {
  GH_TOKEN: string;
  DISPATCH_SECRET?: string;
  GITHUB_REPO?: string;
  GITHUB_WORKFLOW?: string;
  GITHUB_REF?: string;
}

export default {
  async scheduled(event: ScheduledEvent, env: Env, ctx: ExecutionContext): Promise<void> {
    const cron = event.cron;
    const scheduledTime = new Date(event.scheduledTime).toISOString();
    console.log(`[Cron Triggered] cron="${cron}" scheduledTime="${scheduledTime}"`);

    const result = await dispatchWorkflow(env, {
      reason: `Cloudflare Cron Trigger: ${cron}`,
    });

    if (!result.success) {
      console.error(`[Cron Dispatch Failed] ${JSON.stringify(result)}`);
    } else {
      console.log(`[Cron Dispatch Succeeded] HTTP ${result.status}`);
    }
  },

  async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/" || url.pathname === "/status") {
      return new Response(
        JSON.stringify(
          {
            service: "future-meta-cron-dispatcher",
            status: "running",
            utc_time: new Date().toISOString(),
            configured_repo: env.GITHUB_REPO || "zynthium/future-meta",
            configured_workflow: env.GITHUB_WORKFLOW || "update-fee-data.yml",
            configured_ref: env.GITHUB_REF || "main",
            has_token: Boolean(env.GH_TOKEN),
            crons: [
              "35,50 10 * * 1-5 (18:35, 18:50 Asia/Shanghai)",
              "5,20,35 11 * * 1-5 (19:05, 19:20, 19:35 Asia/Shanghai)",
              "2,7,12 0 * * 1-5 (08:02, 08:07, 08:12 Asia/Shanghai)",
              "37 0 * * 1-5 (08:37 Asia/Shanghai)",
              "17 12 * * 1-5 (20:17 Asia/Shanghai)",
            ],
          },
          null,
          2
        ),
        {
          headers: { "Content-Type": "application/json; charset=utf-8" },
        }
      );
    }

    if (url.pathname === "/dispatch" || url.pathname === "/trigger") {
      if (env.DISPATCH_SECRET) {
        const authHeader = request.headers.get("authorization");
        const querySecret = url.searchParams.get("secret");
        const authorized =
          authHeader === `Bearer ${env.DISPATCH_SECRET}` ||
          querySecret === env.DISPATCH_SECRET;
        if (!authorized) {
          return new Response(
            JSON.stringify({ error: "Unauthorized" }),
            { status: 401, headers: { "Content-Type": "application/json" } }
          );
        }
      }

      let inputs: Record<string, unknown> | undefined;
      const isSmoke = url.searchParams.get("smoke") === "true";
      if (isSmoke) {
        inputs = { offline_smoke: true };
      } else if (request.method === "POST") {
        try {
          const body = await request.json() as Record<string, unknown>;
          if (body && typeof body.inputs === "object") {
            inputs = body.inputs as Record<string, unknown>;
          }
        } catch {
          // not json, proceed with empty inputs
        }
      }

      const reason = url.searchParams.get("reason") || "Manual HTTP trigger";
      const result = await dispatchWorkflow(env, { reason, inputs });

      return new Response(JSON.stringify(result, null, 2), {
        status: result.success ? 200 : 502,
        headers: { "Content-Type": "application/json; charset=utf-8" },
      });
    }

    return new Response(JSON.stringify({ error: "Not found" }), {
      status: 404,
      headers: { "Content-Type": "application/json" },
    });
  },
};

async function dispatchWorkflow(
  env: Env,
  options: { reason: string; inputs?: Record<string, unknown> }
): Promise<{ success: boolean; status: number; message: string; details?: unknown }> {
  const repo = env.GITHUB_REPO || "zynthium/future-meta";
  const workflow = env.GITHUB_WORKFLOW || "update-fee-data.yml";
  const ref = env.GITHUB_REF || "main";
  const token = env.GH_TOKEN;

  if (!token) {
    return {
      success: false,
      status: 500,
      message: "Worker missing GH_TOKEN secret",
    };
  }

  const endpoint = `https://api.github.com/repos/${repo}/actions/workflows/${workflow}/dispatches`;
  const payload: Record<string, unknown> = { ref };
  if (options.inputs && Object.keys(options.inputs).length > 0) {
    payload.inputs = options.inputs;
  }

  try {
    const res = await fetch(endpoint, {
      method: "POST",
      headers: {
        Accept: "application/vnd.github+json",
        Authorization: `Bearer ${token}`,
        "User-Agent": "future-meta-cron-dispatcher",
        "X-GitHub-Api-Version": "2022-11-28",
      },
      body: JSON.stringify(payload),
    });

    if (res.status === 204) {
      return {
        success: true,
        status: 204,
        message: `Successfully dispatched ${workflow} on ${ref} (${options.reason})`,
      };
    }

    const text = await res.text();
    let details: unknown = text;
    try {
      details = JSON.parse(text);
    } catch {
      // raw text
    }

    return {
      success: false,
      status: res.status,
      message: `GitHub API returned ${res.status}`,
      details,
    };
  } catch (err: unknown) {
    const errMsg = err instanceof Error ? err.message : String(err);
    return {
      success: false,
      status: 500,
      message: `Network error calling GitHub API: ${errMsg}`,
    };
  }
}
