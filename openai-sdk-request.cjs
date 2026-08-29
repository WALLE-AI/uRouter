const OpenAI = require(
  `${process.env.TEMP}/urouter-openai-sdk/node_modules/openai`,
).default;

async function main() {
  const baseURL = process.env.OPENAI_BASE_URL || "http://127.0.0.1:8787/v1";
  const model = process.env.OPENAI_MODEL || "urouter/auto";
  const stream = process.env.OPENAI_STREAM === "1";
  const client = new OpenAI({
    apiKey: process.env.OPENAI_API_KEY || "local-test",
    baseURL,
    maxRetries: 0,
    timeout: 120_000,
  });

  const models = await client.models.list();
  console.log("SDK models:", models.data.map((model) => model.id).join(", "));

  try {
    const result = await client.chat.completions.create({
      model,
      messages: [{
        role: "user",
        content: process.env.OPENAI_PROMPT || "What New Opportunities Will Inference Models Bring to the Market?",
      }],
      max_tokens: Number(process.env.OPENAI_MAX_TOKENS || 512),
      stream,
    });
    if (!stream) {
      console.log("Reasoning:", result.choices[0].message.reasoning_content || "");
      console.log("Chat response:", result.choices[0].message.content);
      return;
    }
    process.stdout.write("Stream response:\n");
    for await (const chunk of result) {
      const delta = chunk.choices?.[0]?.delta;
      if (delta?.reasoning_content) process.stdout.write(delta.reasoning_content);
      if (delta?.content) process.stdout.write(delta.content);
    }
    process.stdout.write("\n");
  } catch (error) {
    console.log(
      "Chat error:",
      JSON.stringify(
        {
          status: error.status,
          name: error.name,
          message: error.message,
          decisionId: error.headers?.get?.("x-urouter-decision-id"),
        },
        null,
        2,
      ),
    );
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
