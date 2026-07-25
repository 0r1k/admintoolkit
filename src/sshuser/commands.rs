use crate::ssh_exec::escape_single_quotes;

pub fn add_user_commands(username: &str, key: &str) -> Vec<String> {
    vec![
        format!("sudo useradd -m -s /bin/bash {username}"),
        format!("sudo passwd -d {username}"),
        format!("sudo usermod -aG sudo {username}"),
        format!("sudo mkdir -p /home/{username}/.ssh"),
        format!(
            "echo '{}' | sudo tee /home/{username}/.ssh/authorized_keys >/dev/null",
            escape_echo(key)
        ),
        format!("sudo chmod 700 /home/{username}/.ssh"),
        format!("sudo chmod 600 /home/{username}/.ssh/authorized_keys"),
        format!(
            r#"sudo bash -c 'echo -e "StrictHostKeyChecking no\nHost *\n  ForwardAgent yes" > /home/{username}/.ssh/config'"#
        ),
        format!("sudo chown -R {username}:{username} /home/{username}/.ssh"),
        format!(
            "echo '{username} ALL=(ALL:ALL) NOPASSWD: ALL\n' | sudo tee /etc/sudoers.d/{username} >/dev/null"
        ),
        "sudo chmod 755 /etc/sudoers.d".to_string(),
        "sudo chmod 440 /etc/sudoers.d/*".to_string(),
        "sudo visudo -c".to_string(),
    ]
}

pub fn remove_user_commands(username: &str) -> Vec<String> {
    vec![
        format!("sudo deluser {username}"),
        format!("sudo rm -f /etc/sudoers.d/{username}"),
        format!("sudo rm -rf /home/{username}"),
    ]
}

fn escape_echo(s: &str) -> String {
    escape_single_quotes(s)
}

pub fn parse_server_list(raw: &str) -> Vec<String> {
    raw.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}
