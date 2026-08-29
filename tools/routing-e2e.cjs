const OpenAI = require(
  `${process.env.TEMP}/urouter-openai-sdk/node_modules/openai`,
).default;

const client = new OpenAI({
  apiKey: "local-gateway-test",
  baseURL: "http://127.0.0.1:8787/v1",
  maxRetries: 0,
  timeout: 180_000,
});

function contract(id, turn, { hint, role = "primary", preference } = {}) {
  return {
    contract_version: 1,
    task: { id },
    agent: { harness: "routing-e2e" },
    call: { role },
    trace: { turn },
    hint,
    preference,
    data_policy: {
      recording: "metadata_only",
      allow_training: false,
      allow_remote_judge: false,
      retention_days: 1,
    },
  };
}

function routingHeaders(response) {
  return {
    decisionId: response.headers.get("x-urouter-decision-id"),
    tier: response.headers.get("x-urouter-tier"),
    model: response.headers.get("x-urouter-model"),
    reason: response.headers.get("x-urouter-reason"),
  };
}

async function completion(request) {
  return client.chat.completions.create(request).withResponse();
}

async function nonStreamCase(id, prompt, expected, urouter, maxTokens = 96) {
  const started = Date.now();
  const { data, response } = await completion({
    model: "urouter/auto",
    messages: [{ role: "user", content: prompt }],
    max_tokens: maxTokens,
    urouter,
  });
  const route = routingHeaders(response);
  return {
    id,
    pass: route.tier === expected.tier && route.model === expected.model,
    ...route,
    latencyMs: Date.now() - started,
    contentChars: data.choices[0]?.message?.content?.length || 0,
    reasoningChars: data.choices[0]?.message?.reasoning_content?.length || 0,
  };
}

async function streamCase() {
  const started = Date.now();
  const { data: stream, response } = await completion({
    model: "urouter/auto",
    messages: [{ role: "user", content: "Create a concise three-step implementation plan." }],
    max_tokens: 128,
    stream: true,
    stream_options: { include_usage: true },
    urouter: contract("R-008-e2e", "R-008-e2e-turn-1", {
      hint: { workload: "plan" },
    }),
  });
  let contentChars = 0;
  let reasoningChars = 0;
  for await (const chunk of stream) {
    const delta = chunk.choices?.[0]?.delta;
    contentChars += delta?.content?.length || 0;
    reasoningChars += delta?.reasoning_content?.length || 0;
  }
  const route = routingHeaders(response);
  return {
    id: "R-008-stream",
    pass:
      route.tier === "capable" &&
      route.model === "siliconflow/deepseek-r1-pro" &&
      contentChars > 0,
    ...route,
    latencyMs: Date.now() - started,
    contentChars,
    reasoningChars,
  };
}

async function weatherToolLoop() {
  const tools = [
    {
      type: "function",
      function: {
        name: "get_weather",
        description: "Return the weather for a city and date.",
        parameters: {
          type: "object",
          properties: {
            city: { type: "string" },
            date: { type: "string" },
          },
          required: ["city", "date"],
        },
      },
    },
  ];
  const messages = [
    {
      role: "user",
      content: "What is the weather in Wuhan today? Use get_weather before answering.",
    },
  ];
  const started = Date.now();
  const first = await completion({
    model: "urouter/auto",
    messages,
    tools,
    tool_choice: "auto",
    max_tokens: 256,
    urouter: contract("R-004-e2e", "R-004-e2e-turn-1", {
      hint: { difficulty: "hard", workload: "plan" },
    }),
  });
  const firstRoute = routingHeaders(first.response);
  const assistant = first.data.choices[0]?.message;
  const toolCalls = assistant?.tool_calls || [];
  if (toolCalls.length === 0) {
    return {
      id: "R-004-tool-loop",
      pass: false,
      ...firstRoute,
      latencyMs: Date.now() - started,
      toolCalls: 0,
      error: "upstream model did not return a standard OpenAI tool call",
    };
  }

  messages.push({
    role: "assistant",
    content: assistant.content,
    tool_calls: toolCalls,
  });
  for (const toolCall of toolCalls) {
    messages.push({
      role: "tool",
      tool_call_id: toolCall.id,
      content: JSON.stringify({
        city: "Wuhan",
        date: "2026-08-29",
        condition: "cloudy",
        temperature_c: 31,
        source: "routing-test-mock",
      }),
    });
  }

  const second = await completion({
    model: "urouter/auto",
    messages,
    tools,
    max_tokens: 128,
    urouter: contract("R-004-e2e", "R-004-e2e-turn-2", {
      hint: { difficulty: "hard", workload: "plan" },
    }),
  });
  const secondRoute = routingHeaders(second.response);
  const content = second.data.choices[0]?.message?.content || "";
  return {
    id: "R-004-tool-loop",
    pass:
      firstRoute.tier === "capable" &&
      secondRoute.tier === "capable" &&
      firstRoute.model === "siliconflow/deepseek-r1-pro" &&
      secondRoute.model === "siliconflow/deepseek-r1-pro" &&
      content.length > 0,
    first: firstRoute,
    second: secondRoute,
    latencyMs: Date.now() - started,
    toolCalls: toolCalls.length,
    finalContentChars: content.length,
  };
}

async function main() {
  const small = "siliconflow/qwen2.5-7b-instruct";
  const capable = "siliconflow/deepseek-r1-pro";
  const results = [];
  results.push(
    await nonStreamCase(
      "R-001-e2e",
      "Hello. Reply with one short sentence.",
      { tier: "efficient", model: small },
      contract("R-001-e2e", "R-001-e2e-turn-1"),
      32,
    ),
  );
  results.push(
    await nonStreamCase(
      "R-007-e2e",
      "Solve 2x+y=7 and x-y=2, then verify the result.",
      { tier: "capable", model: capable },
      contract("R-007-e2e", "R-007-e2e-turn-1", {
        hint: { difficulty: "hard", workload: "solve" },
      }),
      128,
    ),
  );
  results.push(
    await nonStreamCase(
      "R-009-e2e",
      "Generate a short conversation title.",
      { tier: "efficient", model: small },
      contract("R-009-e2e", "R-009-e2e-turn-1", {
        role: "auxiliary",
        hint: { difficulty: "hard", value_class: "auxiliary" },
      }),
      32,
    ),
  );
  results.push(await streamCase());
  results.push(await weatherToolLoop());
  console.log(JSON.stringify(results, null, 2));
  if (results.some((result) => !result.pass)) process.exitCode = 1;
}

main().catch((error) => {
  console.error(
    JSON.stringify(
      {
        fatal: true,
        status: error.status,
        message: error.message,
        decisionId: error.headers?.get?.("x-urouter-decision-id"),
      },
      null,
      2,
    ),
  );
  process.exitCode = 1;
});
