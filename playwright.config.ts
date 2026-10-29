import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  workers: 1,
  outputDir: process.env.AGENT_GRID_TEST_OUTPUT ?? 'test-results',
  use: { baseURL: 'http://127.0.0.1:1420', viewport: { width: 1440, height: 1000 }, channel: process.env.PLAYWRIGHT_CHANNEL, trace: 'retain-on-failure' },
  webServer: { command: 'npm run dev', url: 'http://127.0.0.1:1420', reuseExistingServer: !process.env.CI },
});
