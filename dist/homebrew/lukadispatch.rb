# Fórmula do tap LLawli/homebrew-tap. A release preenche @VERSAO@ e @SHA256@ e a publica lá;
# este arquivo é o modelo, versionado junto com o código que ele compila.
#
# Compila do código-fonte em vez de reaproveitar o tarball: o binário da release linka a gtk4 do
# sistema, e a do Homebrew mora num prefixo que o carregador não procura.
class Lukadispatch < Formula
  desc "Conversa com as sessões de Claude Code da sua máquina pelo Telegram"
  homepage "https://github.com/LLawli/lukadispatch"
  url "https://github.com/LLawli/lukadispatch/archive/refs/tags/v@VERSAO@.tar.gz"
  sha256 "@SHA256@"
  license "MIT"
  head "https://github.com/LLawli/lukadispatch.git", branch: "master"

  depends_on "pkgconf" => :build
  depends_on "rust" => :build
  depends_on "gtk4"
  depends_on "libadwaita"
  depends_on :linux
  depends_on "tmux"

  def install
    %w[ld-daemon ld-cli ld-ask ld-mcp].each do |crate|
      system "cargo", "install", *std_cargo_args(path: "crates/#{crate}")
    end

    # A unit vem apontando para ~/.local/bin, que é onde o install.sh põe os binários.
    inreplace "dist/lukadispatch.service", "%h/.local/bin", opt_bin
    pkgshare.install "dist/lukadispatch.service", "dist/config.example.toml"
    pkgshare.install ".env.example" => "env.example"
  end

  def caveats
    <<~EOS
      O daemon roda como serviço de usuário do systemd:
        mkdir -p ~/.config/systemd/user ~/.config/lukadispatch
        ln -sf #{opt_pkgshare}/lukadispatch.service ~/.config/systemd/user/
        cp -n #{opt_pkgshare}/config.example.toml ~/.config/lukadispatch/config.toml
        [ -e ~/.config/lukadispatch/.env ] || install -m600 #{opt_pkgshare}/env.example ~/.config/lukadispatch/.env

      Preencha o token e o chat_id no .env e o [telegram] no config.toml (o README explica
      como criar o bot), e então:
        lukadispatch install --global
        systemctl --user enable --now lukadispatch
    EOS
  end

  test do
    assert_match "lukadispatch", shell_output("#{bin}/lukadispatch --help 2>&1")
  end
end
