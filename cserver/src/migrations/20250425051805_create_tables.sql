-- Add migration script here

-- ユーザーロールのENUM型を作成
CREATE TYPE user_role AS ENUM ('admin', 'user');

-- ユーザーテーブルを作成
CREATE TABLE users (
    id UUID PRIMARY KEY,                     -- UUID v7 (アプリケーション側で生成)
    username TEXT UNIQUE NOT NULL,           -- ユーザー名 (一意)
    password_hash TEXT NOT NULL,             -- ハッシュ化されたパスワード
    role user_role NOT NULL DEFAULT 'user',  -- ユーザーロール (デフォルトは 'user')
    created_at TIMESTAMPTZ NOT NULL          -- 登録日時 (アプリケーション側で生成)
);

-- トークンテーブルを作成
CREATE TABLE tokens (
    token TEXT PRIMARY KEY,                  -- トークン文字列
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE, -- ユーザーID (usersテーブルへの外部キー)
    expires_at TIMESTAMPTZ NOT NULL,         -- 有効期限 (アプリケーション側で生成)
    created_at TIMESTAMPTZ NOT NULL          -- 登録日時 (アプリケーション側で生成)
);

-- ユーザポイントテーブルを作成
CREATE TABLE user_points (
    user_id UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE, -- ユーザーID (usersテーブルへの外部キー)
    points INTEGER NOT NULL DEFAULT 0 CHECK (points >= 0),        -- 保有ポイント (デフォルト0, 負数不可)
    updated_at TIMESTAMPTZ NOT NULL          -- 更新日時 (アプリケーション側で生成)
);

-- インデックスの作成 (任意ですが、パフォーマンス向上のために推奨)
CREATE INDEX idx_tokens_user_id ON tokens(user_id);
CREATE INDEX idx_tokens_expires_at ON tokens(expires_at);
