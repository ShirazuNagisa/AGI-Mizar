#!/usr/bin/env node
/**
 * A stand-in for `codex app-server`, for the Rust tests.
 *
 * Speaks the same newline-delimited JSON-RPC: answers the calls the adapter
 * makes, volunteers the notifications a real server would, and — the part that
 * matters — asks for an approval and records the decision it receives, so the
 * test can assert the answer actually went back.
 *
 * argv[2] is a file to append what it received to.
 */
import { appendFileSync } from 'node:fs'
import { createInterface } from 'node:readline'

const record = process.argv[2]

const thread = {
  id: 'th_1',
  name: 'Fix the parser',
  cwd: '/tmp/work',
  model: 'gpt-5',
  updatedAt: 1700000000,
  status: { type: 'idle' },
}
const other = {
  id: 'th_2',
  preview: 'what is this codebase',
  updatedAt: 1699990000,
  status: { type: 'active' },
}
const turn = {
  id: 't_1',
  status: 'completed',
  startedAt: 1700000000,
  items: [
    { type: 'userMessage', content: [{ type: 'text', text: 'list the files' }] },
    { type: 'commandExecution', id: 'c1', command: 'ls', aggregatedOutput: 'a.txt' },
    { type: 'agentMessage', text: 'one file' },
  ],
}

const send = (frame) => process.stdout.write(`${JSON.stringify(frame)}\n`)
const note = (method, params) => send({ method, params })
const remember = (entry) => {
  if (record !== undefined) appendFileSync(record, `${JSON.stringify(entry)}\n`)
}

createInterface({ input: process.stdin }).on('line', (line) => {
  let message
  try {
    message = JSON.parse(line)
  } catch {
    return
  }

  // An answer to a request this server made.
  if (message.id === 'srv-1' && message.method === undefined) {
    remember({ answered: message.result })
    note('turn/completed', { threadId: 'th_1', turn: { id: 't_new', status: 'completed' } })
    return
  }

  if (message.id === 'srv-2' && message.method === undefined) {
    remember({ question_answered: message.result })
    return
  }

  switch (message.method) {
    case 'initialize':
      send({
        id: message.id,
        result: { userAgent: 'codex_cli_rs/0.5.0 (macos)', platformOs: 'macos' },
      })
      return
    case 'thread/list':
      send({ id: message.id, result: { data: [thread, other] } })
      return
    case 'thread/turns/list':
      send({ id: message.id, result: { data: [turn] } })
      return
    case 'thread/resume':
      send({ id: message.id, result: { thread } })
      return
    case 'model/list':
      send({
        id: message.id,
        result: {
          data: [
            {
              id: 'deepseek-flash',
              displayName: 'deepseek-flash',
              supportedReasoningEfforts: [{ reasoningEffort: 'max', description: 'hardest' }],
            },
            { id: 'plain', displayName: 'Plain' },
          ],
          nextCursor: null,
        },
      })
      return
    case 'permissionProfile/list':
      send({
        id: message.id,
        result: {
          data: [
            { id: ':read-only', description: 'Read only', allowed: true },
            { id: ':workspace', description: null, allowed: true },
          ],
          nextCursor: null,
        },
      })
      return
    case 'thread/start':
      remember({ started: message.params })
      send({
        id: message.id,
        result: { thread: { id: 'th_new', cwd: message.params.cwd ?? '/tmp/new' } },
      })
      return
    case 'thread/name/set':
      remember({ renamed: message.params })
      send({ id: message.id, result: {} })
      return
    case 'thread/archive':
      if (message.params.threadId === 'th_empty') {
        send({
          id: message.id,
          error: { code: -32600, message: 'no rollout found for thread id th_empty' },
        })
        return
      }
      remember({ archived: message.params })
      send({ id: message.id, result: {} })
      return
    case 'thread/read':
      send({
        id: message.id,
        result: {
          thread: {
            id: message.params.threadId,
            cwd: '/Users/example/project',
            path: '/tmp/rollout-does-not-exist.jsonl',
            ...(message.params.includeTurns ? { turns: [turn] } : {}),
          },
        },
      })
      return
    case 'thread/settings/update':
      remember({ settings: message.params })
      send({ id: message.id, result: {} })
      return
    case 'turn/start':
      send({ id: message.id, result: { turn: { id: 't_new' } } })
      note('turn/started', {
        threadId: message.params.threadId,
        turn: { id: 't_new', status: 'inProgress' },
      })
      note('item/fileChange/patchUpdated', {
        threadId: message.params.threadId,
        turnId: 't_new',
        itemId: 'i3',
        changes: [
          {
            path: '/Users/example/project/src/a.rs',
            kind: 'update',
            diff: '@@ -1 +1 @@',
          },
        ],
      })
      // The approval a real Codex asks for before running a shell command.
      send({
        id: 'srv-1',
        method: 'item/commandExecution/requestApproval',
        params: {
          threadId: message.params.threadId,
          itemId: 'c9',
          reason: 'wants to run rm -rf build',
        },
      })
      send({
        id: 'srv-2',
        method: 'item/tool/requestUserInput',
        params: {
          threadId: message.params.threadId,
          turnId: 't_new',
          itemId: 'i2',
          isBlocking: true,
          questions: [
            {
              id: 'q1',
              header: 'Pick one',
              question: 'Which build should run?',
              options: [{ label: 'Alpha' }, { label: 'Beta' }],
            },
          ],
        },
      })
      return
    case 'turn/interrupt':
      remember({ interrupted: message.params })
      send({ id: message.id, result: {} })
      return
    default:
      if (message.id !== undefined) send({ id: message.id, result: {} })
  }
})
