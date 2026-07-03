pub const LOGO: &str = r#"
                         |
                         |  trelane
                    \    |    /
                     \   |   /
                      \  |  /
                       \ | /
                        \|/
                   _____ _|_____
                  |     | |     |__
              ____|_____|_|_____|__|___
             /    |           /       | \
            /_____|__________/________|__\
           [==|_____________]_[_________]_]
          /  /  _\___/_      |  |  |  |
         /__/___[_____]______|__|__|__|
                \___/
     ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
"#;

pub const LOGO_SMALL: &str = r#"
         |  trelane
    \    |    /
     \   |   /
      \  |  /
       \ | /
        \|/
   _____|_____
  |    _|--_--_
  |___/ | \____\
  [_____|___]___]
   /  |_|\
  ~~~~~~~~~~~~~~~
"#;

pub fn print_logo() {
    println!("{}", LOGO);
}

pub fn print_splash(agent: &str, reason: &str, root: &str) {
    println!("\n{}", LOGO_SMALL);
    println!("  Agent   : {}", agent);
    println!("  Reason  : {}", reason);
    println!("  Project : {}", root);
    println!("  Status  : launching...\n");
}
