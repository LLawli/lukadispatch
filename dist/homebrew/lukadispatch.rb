# Fórmula do tap LLawli/homebrew-tap. A release preenche a versão e os sha256 e a publica lá;
# este arquivo é o modelo, versionado junto com o código.
#
# Reempacota o tarball da release, o mesmo que o install.sh e o mise instalam: o brew não
# compila nada. Daemon, CLI e proxy MCP vêm estáticos (musl) e rodam em qualquer Linux; a janela
# de pergunta do PC linka a gtk4 e a libadwaita do sistema, e sem elas a pergunta segue pelo chat.
class Lukadispatch < Formula
  desc "Conversa com as sessões de Claude Code da sua máquina pelo Telegram"
  homepage "https://github.com/LLawli/lukadispatch"
  version "@VERSAO@"
  license "MIT"

  depends_on :linux
  depends_on "tmux"

  on_linux do
    on_intel do
      url "https://github.com/LLawli/lukadispatch/releases/download/v@VERSAO@/lukadispatch-linux-x86_64.tar.gz"
      sha256 "@SHA_X86_64@"
    end
    on_arm do
      url "https://github.com/LLawli/lukadispatch/releases/download/v@VERSAO@/lukadispatch-linux-aarch64.tar.gz"
      sha256 "@SHA_AARCH64@"
    end
  end

  def install
    bin.install "lukadispatchd", "lukadispatch", "lukadispatch-ask", "lukadispatch-mcp"
    # A unit aponta para o opt/, que o brew mantém entre versões; a pasta da versão some no
    # próximo upgrade.
    inreplace "lukadispatch.service", "%h/.local/bin", opt_bin
    pkgshare.install "lukadispatch.service", "config.example.toml", "env.example"
  end

  def caveats
    <<~EOS
      O setup cria o bot, o grupo e o config conversando, e liga o serviço:
        lukadispatch setup

      Depois de um brew upgrade, reinicie o serviço para ele rodar a versão nova:
        systemctl --user restart lukadispatch
    EOS
  end

  test do
    assert_match "lukadispatch", shell_output("#{bin}/lukadispatch --help 2>&1")
    assert_match "setup", shell_output("#{bin}/lukadispatchd setup --help 2>&1")
  end
end
