pub fn solve_normal(a: &[f64], b: &[f64], n: usize) -> Option<Vec<f64>> {
    let mut l = vec![0.0; n * n];
    for i in 0..n {
        for j in 0..=i {
            let mut sum = a[i * n + j];
            for k in 0..j {
                sum -= l[i * n + k] * l[j * n + k];
            }
            if i == j {
                if sum <= 1e-10 {
                    return None;
                }
                l[i * n + i] = sum.sqrt();
            } else {
                l[i * n + j] = sum / l[j * n + j];
            }
        }
    }
    let mut y = vec![0.0; n];
    for i in 0..n {
        let mut sum = b[i];
        for j in 0..i {
            sum -= l[i * n + j] * y[j];
        }
        y[i] = sum / l[i * n + i];
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let mut sum = y[i];
        for j in (i + 1)..n {
            sum -= l[j * n + i] * x[j];
        }
        x[i] = sum / l[i * n + i];
    }
    Some(x)
}

pub fn inverse_symmetric(a: &[f64], n: usize) -> Option<Vec<f64>> {
    let mut columns = vec![0.0; n * n];
    for col in 0..n {
        let mut target = vec![0.0; n];
        target[col] = 1.0;
        let solved = solve_normal(a, &target, n)?;
        for row in 0..n {
            columns[row * n + col] = solved[row];
        }
    }
    Some(columns)
}
