use logos::Logos;
use eframe::egui;

#[derive(Logos, Debug, PartialEq)]
pub enum PythonToken {
    #[regex(r"def|class|if|else|elif|for|while|try|except|finally|with|as|return|break|continue|pass|import|from|in|is|and|or|not|global|nonlocal|lambda|yield|raise|assert|del|True|False|None")]
    Keyword,

    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*")]
    Ident,

    #[regex(r"0[xX][0-9a-fA-F]+|\d+(\.\d+)?([eE][+-]?\d+)?")]
    Number,

    #[regex(r#"("[^"\\]*(?:\\.[^"\\]*)*")|('[^'\\]*(?:\\.[^'\\]*)*')"#)]
    String,

    #[regex(r"#.*")]
    Comment,

    #[regex(r"[\(\)\[\]\{\}\.,:;@=+\-*/%&|^~<>]")]
    Punctuation,
    
    #[regex(r"[ \t\n\f\r]+")]
    Whitespace,

}

#[derive(Default)]
pub struct Highlighter {}

impl Highlighter {
    pub fn highlight(&self, text: &str, wrap_width: f32) -> egui::text::LayoutJob {
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = wrap_width;

        let mut lexer = PythonToken::lexer(text);
        let mut last_end = 0;

        while let Some(token_res) = lexer.next() {
            let span = lexer.span();
            
            if span.start > last_end {
                job.append(
                    &text[last_end..span.start],
                    0.0,
                    Self::format_for(None),
                );
            }

            let token = token_res.ok();
            job.append(
                &text[span.clone()],
                0.0,
                Self::format_for(token.as_ref()),
            );

            last_end = span.end;
        }

        if last_end < text.len() {
            job.append(
                &text[last_end..],
                0.0,
                Self::format_for(None),
            );
        }

        job
    }

    fn format_for(token: Option<&PythonToken>) -> egui::text::TextFormat {
        let color = match token {
            Some(PythonToken::Keyword) => egui::Color32::from_rgb(198, 120, 221),
            Some(PythonToken::Ident) => egui::Color32::from_rgb(97, 175, 239),
            Some(PythonToken::Number) => egui::Color32::from_rgb(209, 154, 102),
            Some(PythonToken::String) => egui::Color32::from_rgb(152, 195, 121),
            Some(PythonToken::Comment) => egui::Color32::from_rgb(92, 99, 112),
            Some(PythonToken::Punctuation) => egui::Color32::from_rgb(171, 178, 191),
            Some(PythonToken::Whitespace) | None => egui::Color32::from_rgb(171, 178, 191),
        };
        egui::text::TextFormat::simple(egui::FontId::monospace(14.0), color)
    }
}
