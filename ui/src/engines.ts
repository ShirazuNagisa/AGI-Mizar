import type { EngineId } from './api'

/**
 * The facts about the three agents that are the same wherever they are shown.
 *
 * The guide and the agent pages both have to say "this one is not here" and
 * "this one needs a plugin", and they must say it the same way — one copy, so
 * the two screens cannot drift.
 */

/** The plugin the desktop host speaks to inside DSH. */
export const DSH_PLUGIN_VERSION = '0.9.0'

/** What a person pastes into the DSH terminal to add it. */
export const DSH_PLUGIN_COMMAND =
  'dsh plugin --profile desktop add github:ShirazuNagisa/DSH-Mizar#0.9.0'

/** One line each: what to install before this window can use the agent. */
export const ENGINE_INSTALL_HINTS: Record<EngineId, string> = {
  dsh: '先安装 DeepSeek Harness（其中带 dsh 命令行），再回到这里重新检测。',
  claude: '在终端运行 npm install -g @anthropic-ai/claude-code，再回到这里重新检测。',
  codex: '安装 ChatGPT 应用，或在终端运行 npm i -g @openai/codex，再回到这里重新检测。',
}
