-- OpenAI API list prices (USD per 1M tokens, standard tier, <=272K input),
-- from https://developers.openai.com/api/docs/pricing as of 2026-10-13.
-- These are API-equivalent estimates: Codex/ChatGPT-OAuth traffic is billed
-- as a subscription, not per token. Edit via PUT /admin/usage/prices.
-- Models with no cached rate on that page reuse the input price.
INSERT OR IGNORE INTO model_prices (model, input_per_mtok, output_per_mtok, cached_per_mtok) VALUES
 ('gpt-6-astra', 10.00, 50.00, 1.00),
 ('gpt-6.1-sol', 2.00, 10.00, 0.10),
 ('gpt-6-luna', 0.10, 0.50, 0.01),
 ('gpt-6-sol', 2.00, 10.00, 0.20),
 ('gpt-5.6-sol', 4.00, 20.00, 0.40),
 ('gpt-5.6-terra', 2.00, 12.00, 0.20),
 ('gpt-5.6-luna', 0.20, 1.20, 0.02),
 ('gpt-5.5', 5.00, 30.00, 0.50),
 ('gpt-5.5-pro', 30.00, 180.00, NULL),
 ('gpt-5.4', 2.50, 15.00, 0.25),
 ('gpt-5.4-mini', 0.75, 4.50, 0.075),
 ('gpt-5.4-nano', 0.20, 1.25, 0.02),
 ('gpt-5.4-pro', 30.00, 180.00, NULL),
 ('gpt-5.3-codex', 1.75, 14.00, 0.175),
 ('gpt-5.2', 1.75, 14.00, 0.175),
 ('gpt-5.2-pro', 21.00, 168.00, NULL),
 ('gpt-5.1', 1.25, 10.00, 0.125),
 ('gpt-5', 1.25, 10.00, 0.125),
 ('gpt-5-mini', 0.25, 2.00, 0.025),
 ('gpt-5-nano', 0.05, 0.40, 0.005),
 ('gpt-5-pro', 15.00, 120.00, NULL),
 ('gpt-4.1', 2.00, 8.00, 0.50),
 ('gpt-4.1-mini', 0.40, 1.60, 0.10),
 ('gpt-4.1-nano', 0.10, 0.40, 0.025),
 ('gpt-4o', 2.50, 10.00, 1.25),
 ('gpt-4o-mini', 0.15, 0.60, 0.075),
 ('o3', 2.00, 8.00, 0.50),
 ('o4-mini', 1.10, 4.40, 0.275);
